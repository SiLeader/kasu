use super::*;
use opentelemetry::trace::{SpanKind, TraceContextExt};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use std::sync::{Arc, Mutex};
#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Vec<SpanData>>>);
impl SpanExporter for Capture {
    async fn export(&self, batch: Vec<SpanData>) -> opentelemetry_sdk::error::OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}
fn subscriber(capture: Capture) -> (SdkTracerProvider, impl tracing::Subscriber + Send + Sync) {
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(capture)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
        .build();
    let layer = tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("test"))
        .with_location(false)
        .with_threads(false)
        .with_filter(tracing_subscriber::filter::filter_fn(|m| {
            m.is_span() && m.target() == "kasu_telemetry"
        }));
    let subscriber = tracing_subscriber::registry().with(layer);
    (provider, subscriber)
}
fn config(values: &[(&str, &str)]) -> Result<Option<Config>, Error> {
    Config::read(|key| {
        Ok(values
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string()))
    })
}
#[test]
fn configuration_is_opt_in_validated_and_errors_are_sanitized() {
    assert!(config(&[]).unwrap().is_none());
    assert!(
        config(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "invalid")])
            .unwrap()
            .is_none()
    );
    assert!(
        config(&[
            ("OTEL_TRACES_EXPORTER", "otlp"),
            ("OTEL_SDK_DISABLED", "true")
        ])
        .unwrap()
        .is_none()
    );
    let enabled = [("OTEL_TRACES_EXPORTER", "otlp")];
    assert_eq!(
        config(&enabled).unwrap().unwrap().endpoint,
        "http://localhost:4318/v1/traces"
    );
    let base = [
        enabled.as_slice(),
        &[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector/base/")],
    ]
    .concat();
    assert_eq!(
        config(&base).unwrap().unwrap().endpoint,
        "https://collector/base/v1/traces"
    );
    let exact = [
        base.as_slice(),
        &[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "https://collector/custom",
        )],
    ]
    .concat();
    assert_eq!(
        config(&exact).unwrap().unwrap().endpoint,
        "https://collector/custom"
    );
    for (key, value) in [
        ("OTEL_TRACES_EXPORTER", "console"),
        ("OTEL_SDK_DISABLED", "maybe"),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ("OTEL_TRACES_SAMPLER", "arbitrary"),
        (
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://user:secret@host/path",
        ),
        (
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "https://host/?secret=1",
        ),
        ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "file:///secret"),
    ] {
        let values = if key == "OTEL_TRACES_EXPORTER" {
            vec![(key, value)]
        } else {
            vec![("OTEL_TRACES_EXPORTER", "otlp"), (key, value)]
        };
        let error = config(&values)
            .err()
            .expect("must reject invalid configuration")
            .to_string();
        assert!(!error.contains("secret"));
    }
    for value in ["NaN", "inf", "-0.1", "1.1", "invalid", ""] {
        assert!(
            config(&[
                ("OTEL_TRACES_EXPORTER", "otlp"),
                ("OTEL_TRACES_SAMPLER", "traceidratio"),
                ("OTEL_TRACES_SAMPLER_ARG", value)
            ])
            .is_err()
        );
    }
    assert!(
        config(&[
            ("OTEL_TRACES_EXPORTER", "otlp"),
            ("OTEL_TRACES_SAMPLER", "parentbased_traceidratio"),
            ("OTEL_TRACES_SAMPLER_ARG", "0.25")
        ])
        .is_ok()
    );
}
#[test]
fn remote_parent_child_injection_sampling_and_event_exclusion() {
    let captured = Capture::default();
    let (_provider, subscriber) = subscriber(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        let parent = "00-11111111111111111111111111111111-2222222222222222-01";
        let span = server_span("GetBlobs", "grpc", |key| match key {
            "traceparent" => Some(parent),
            "tracestate" => Some("vendor=value"),
            "baggage" => Some("secret=data"),
            _ => None,
        });
        let parent_id = span.context().span().span_context().span_id();
        let _entered = span.enter();
        let child = client_span("GetBlobs", "grpc");
        {
            let _child = child.enter();
            let mut metadata = tonic::metadata::MetadataMap::new();
            inject(&mut metadata);
            let outgoing = metadata.get("traceparent").unwrap().to_str().unwrap();
            assert!(outgoing.starts_with("00-11111111111111111111111111111111-"));
            assert!(!outgoing.contains("2222222222222222"));
            assert_eq!(metadata.get("tracestate").unwrap(), "vendor=value");
            assert!(metadata.get("baggage").is_none());
            tracing::warn!(secret = "must never appear in OTLP events");
        }
        finish(&child, tonic::Code::PermissionDenied);
        drop(child);
        assert_eq!(captured.0.lock().unwrap()[0].parent_span_id, parent_id);
        finish(&span, tonic::Code::Ok);
        drop(_entered);
        drop(span);
        let unsampled = server_span("Log", "http", |_| {
            Some("00-33333333333333333333333333333333-4444444444444444-00")
        });
        {
            let _entered = unsampled.enter();
            let child = client_span("Log", "grpc");
            let _child = child.enter();
            let mut metadata = tonic::metadata::MetadataMap::new();
            inject(&mut metadata);
            assert!(
                metadata
                    .get("traceparent")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .ends_with("-00")
            );
        }
    });
    let spans = captured.0.lock().unwrap();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].span_kind, SpanKind::Client);
    assert_eq!(spans[1].span_kind, SpanKind::Server);
    assert!(spans[1].parent_span_is_remote);
    assert_eq!(spans[1].parent_span_id.to_string(), "2222222222222222");
    assert!(spans.iter().all(|s| s.events.is_empty()));
    assert!(
        spans[0]
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "kasu.code" && kv.value.to_string() == "7")
    );
}
#[test]
fn malformed_and_oversized_contexts_start_new_roots() {
    let captured = Capture::default();
    let (_provider, subscriber) = subscriber(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        for parent in [
            "not-a-traceparent".to_string(),
            "00-00000000000000000000000000000000-2222222222222222-01".into(),
            "a".repeat(513),
        ] {
            let span = server_span("Log", "http", |_| Some(parent.as_str()));
            drop(span);
        }
    });
    let spans = captured.0.lock().unwrap();
    assert_eq!(spans.len(), 3);
    assert!(spans.iter().all(
        |s| s.parent_span_id == opentelemetry::trace::SpanId::INVALID
            && s.span_context.trace_id().to_string() != "00000000000000000000000000000000"
    ));
}
#[tokio::test]
async fn grpc_span_survives_headers_and_closes_as_cancelled_on_body_drop() {
    use http_body_util::{BodyExt, Full};
    use tower::{Layer, ServiceExt};
    let captured = Capture::default();
    let (_provider, subscriber) = subscriber(captured.clone());
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let service = GrpcLayer.layer(tower::service_fn(|_: http::Request<()>| async {
        Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(
            bytes::Bytes::from_static(b"data"),
        )))
    }));
    let response = service
        .clone()
        .oneshot(
            http::Request::builder()
                .uri("/kasu.v1.Objects/GetBlobs")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(captured.0.lock().unwrap().is_empty());
    drop(response);
    assert!(
        captured.0.lock().unwrap()[0]
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "kasu.code" && kv.value.to_string() == "1")
    );
    let response = service
        .oneshot(
            http::Request::builder()
                .uri("/kasu.v1.Objects/GetTrees")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    response.into_body().collect().await.unwrap();
    assert!(
        captured.0.lock().unwrap()[1]
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "kasu.code" && kv.value.to_string() == "0")
    );
}

#[tokio::test]
async fn blocking_worker_retains_span_and_current_context_after_caller_drops_it() {
    let captured = Capture::default();
    let (_provider, subscriber) = subscriber(captured.clone());
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let span = operation_span("blocking");
    let id = span.context().span().span_context().span_id();
    let (started, wait) = tokio::sync::oneshot::channel();
    let (release, receive) = std::sync::mpsc::channel();
    let worker = {
        let _entered = span.enter();
        blocking(move || {
            let worker_id = Span::current().context().span().span_context().span_id();
            started.send(worker_id).unwrap();
            receive.recv_timeout(Duration::from_secs(3)).unwrap();
        })
    };
    assert_eq!(wait.await.unwrap(), id);
    drop(span);
    assert!(captured.0.lock().unwrap().is_empty());
    release.send(()).unwrap();
    worker.await.unwrap();
    assert_eq!(captured.0.lock().unwrap().len(), 1);
}
#[test]
fn full_export_queue_drops_spans_without_waiting_for_exporter() {
    use std::sync::atomic::{AtomicBool, Ordering};
    #[derive(Debug)]
    struct Blocked {
        capture: Capture,
        first: AtomicBool,
        started: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl SpanExporter for Blocked {
        async fn export(&self, batch: Vec<SpanData>) -> opentelemetry_sdk::error::OTelSdkResult {
            if self.first.swap(false, Ordering::Relaxed) {
                self.started.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            self.capture.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }
    let captured = Capture::default();
    let (started, wait) = std::sync::mpsc::channel();
    let (release, receive) = std::sync::mpsc::channel();
    let exporter = Blocked {
        capture: captured.clone(),
        first: AtomicBool::new(true),
        started,
        release: Mutex::new(receive),
    };
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(4)
        .with_max_export_batch_size(1)
        .with_scheduled_delay(Duration::from_millis(1))
        .build();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_span_processor(
            BatchSpanProcessor::builder(exporter)
                .with_batch_config(batch)
                .build(),
        )
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("queue")));
    tracing::subscriber::with_default(subscriber, || {
        drop(operation_span("first"));
        wait.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            drop(operation_span("overflow"));
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        release.send(()).unwrap();
    });
    provider
        .shutdown_with_timeout(Duration::from_secs(3))
        .unwrap();
    let exported = captured.0.lock().unwrap().len();
    assert!(
        (1..=5).contains(&exported),
        "exported {exported} despite a blocked four-slot queue"
    );
}
