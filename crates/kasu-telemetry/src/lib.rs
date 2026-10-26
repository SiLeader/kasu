//! Bounded, opt-in OTLP tracing and W3C trace-context propagation.
mod transport;
use opentelemetry::{
    Context,
    propagation::{Extractor, Injector, TextMapPropagator},
    trace::TracerProvider,
};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    Resource,
    propagation::TraceContextPropagator,
    trace::{BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider},
};
use std::{fmt, time::Duration};
use tracing::{Span, field::Empty};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
pub use transport::{GrpcLayer, GrpcService};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
#[derive(Debug)]
struct Invalid(&'static str);
impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Invalid {}
fn invalid(message: &'static str) -> Error {
    Box::new(Invalid(message))
}

struct Config {
    endpoint: String,
    sampler: Sampler,
}
impl Config {
    fn read(get: impl Fn(&str) -> Result<Option<String>, Error>) -> Result<Option<Self>, Error> {
        if let Some(disabled) = get("OTEL_SDK_DISABLED")? {
            match disabled.as_str() {
                "true" => return Ok(None),
                "false" => {}
                _ => return Err(invalid("invalid OTEL_SDK_DISABLED")),
            }
        }
        match get("OTEL_TRACES_EXPORTER")?.as_deref().unwrap_or("none") {
            "none" => return Ok(None),
            "otlp" => {}
            _ => return Err(invalid("OTEL_TRACES_EXPORTER must be none or otlp")),
        }
        let protocol =
            get("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")?.or(get("OTEL_EXPORTER_OTLP_PROTOCOL")?);
        if protocol.as_deref().is_some_and(|p| p != "http/protobuf") {
            return Err(invalid("only OTLP http/protobuf is supported"));
        }
        let endpoint = if let Some(endpoint) = get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")? {
            endpoint
        } else {
            format!(
                "{}/v1/traces",
                get("OTEL_EXPORTER_OTLP_ENDPOINT")?
                    .unwrap_or_else(|| "http://localhost:4318".into())
                    .trim_end_matches('/')
            )
        };
        let url =
            reqwest::Url::parse(&endpoint).map_err(|_| invalid("invalid OTLP trace endpoint"))?;
        if endpoint.len() > 4096
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "OTLP endpoint must be HTTP(S) without credentials, query or fragment",
            ));
        }
        let sampler = get("OTEL_TRACES_SAMPLER")?.unwrap_or_else(|| "parentbased_always_on".into());
        let ratio = || -> Result<f64, Error> {
            let value = get("OTEL_TRACES_SAMPLER_ARG")?
                .ok_or_else(|| invalid("missing OTEL_TRACES_SAMPLER_ARG"))?
                .parse::<f64>()
                .map_err(|_| invalid("invalid trace sampling ratio"))?;
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(invalid("invalid trace sampling ratio"));
            }
            Ok(value)
        };
        let sampler = match sampler.as_str() {
            "always_on" => Sampler::AlwaysOn,
            "always_off" => Sampler::AlwaysOff,
            "parentbased_always_on" => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
            "parentbased_always_off" => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
            "traceidratio" => Sampler::TraceIdRatioBased(ratio()?),
            "parentbased_traceidratio" => {
                Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio()?)))
            }
            _ => return Err(invalid("unsupported OTEL_TRACES_SAMPLER")),
        };
        Ok(Some(Self { endpoint, sampler }))
    }
}
/// Keep this guard until application spans have closed. Shutdown flushes with a finite deadline.
pub struct Guard(Option<SdkTracerProvider>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(provider) = self.0.take()
            && provider
                .shutdown_with_timeout(Duration::from_secs(3))
                .is_err()
        {
            eprintln!("kasu: telemetry shutdown did not finish successfully");
        }
    }
}

/// Initialize stderr logging and optional OTLP tracing. Exporter errors never expose configuration values.
pub fn init(service: &'static str) -> Result<Guard, Error> {
    let config = Config::read(|key| match std::env::var(key) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(invalid("telemetry environment must be UTF-8")),
    })?;
    let provider = if let Some(config) = config {
        // reqwest's blocking client owns a runtime. Build it outside an async runtime.
        Some(
            std::thread::spawn(move || -> Result<SdkTracerProvider, Error> {
                let client = reqwest::blocking::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(1))
                    .timeout(Duration::from_secs(2))
                    .build()
                    .map_err(|_| invalid("cannot initialize telemetry HTTP client"))?;
                let exporter = opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .with_http_client(client)
                    .with_endpoint(config.endpoint)
                    .with_protocol(Protocol::HttpBinary)
                    .with_timeout(Duration::from_secs(2))
                    .build()
                    .map_err(|_| invalid("cannot initialize OTLP trace exporter"))?;
                let batch = BatchConfigBuilder::default()
                    .with_max_queue_size(2048)
                    .with_max_export_batch_size(128)
                    .with_scheduled_delay(Duration::from_secs(1))
                    .build();
                Ok(SdkTracerProvider::builder()
                    .with_span_processor(
                        BatchSpanProcessor::builder(exporter)
                            .with_batch_config(batch)
                            .build(),
                    )
                    .with_sampler(config.sampler)
                    .with_max_attributes_per_span(16)
                    .with_max_events_per_span(0)
                    .with_max_links_per_span(0)
                    .with_resource(
                        Resource::builder_empty()
                            .with_service_name(service)
                            .with_attributes([opentelemetry::KeyValue::new(
                                "service.version",
                                env!("CARGO_PKG_VERSION"),
                            )])
                            .build(),
                    )
                    .build())
            })
            .join()
            .map_err(|_| invalid("telemetry initialization worker failed"))??,
        )
    } else {
        None
    };
    let telemetry = provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("kasu"))
            .with_location(false)
            .with_threads(false)
            .with_filter(tracing_subscriber::filter::filter_fn(|m| {
                m.is_span() && m.target() == "kasu_telemetry"
            }))
    });
    let logging = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        );
    let guard = Guard(provider);
    tracing_subscriber::registry()
        .with(telemetry)
        .with(logging)
        .try_init()
        .map_err(|_| invalid("cannot initialize telemetry subscriber"))?;
    Ok(guard)
}

struct Incoming {
    traceparent: Option<String>,
    tracestate: Option<String>,
}
impl Extractor for Incoming {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "traceparent" => self.traceparent.as_deref(),
            "tracestate" => self.tracestate.as_deref(),
            _ => None,
        }
    }
    fn keys(&self) -> Vec<&str> {
        vec!["traceparent", "tracestate"]
    }
}
/// Accept only bounded W3C trace context. Baggage is deliberately not propagated.
pub fn server_span<'a>(
    method: &'static str,
    transport: &'static str,
    get: impl Fn(&str) -> Option<&'a str>,
) -> Span {
    let span = tracing::info_span!(target:"kasu_telemetry",parent:None,"server_request",method,transport,otel.kind="server",otel.status_code=Empty,kasu.code=Empty);
    let context = TraceContextPropagator::new().extract_with_context(
        &Context::new(),
        &Incoming {
            traceparent: get("traceparent")
                .filter(|v| v.len() <= 512)
                .map(str::to_owned),
            tracestate: get("tracestate")
                .filter(|v| v.len() <= 512)
                .map(str::to_owned),
        },
    );
    let _ = span.set_parent(context);
    span
}
pub fn client_span(method: &'static str, transport: &'static str) -> Span {
    tracing::info_span!(target:"kasu_telemetry","client_request",method,transport,otel.kind="client",otel.status_code=Empty,kasu.code=Empty)
}
pub fn operation_span(operation: &'static str) -> Span {
    tracing::info_span!(target:"kasu_telemetry","operation",operation,otel.status_code=Empty,kasu.code=Empty)
}
pub fn finish(span: &Span, code: tonic::Code) {
    span.record("kasu.code", code as i64);
    span.record(
        "otel.status_code",
        if code == tonic::Code::Ok {
            "OK"
        } else {
            "ERROR"
        },
    );
}
struct Metadata<'a>(&'a mut tonic::metadata::MetadataMap);
impl Injector for Metadata<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(key), Ok(value)) = (
            key.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>(),
            value.parse(),
        ) {
            self.0.insert(key, value);
        }
    }
}
pub fn inject(metadata: &mut tonic::metadata::MetadataMap) {
    TraceContextPropagator::new()
        .inject_context(&Span::current().context(), &mut Metadata(metadata));
}
/// Preserve the caller's span when moving synchronous work to Tokio's blocking pool.
pub fn blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let span = Span::current();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            let _entered = span.enter();
            work()
        })
    })
}

#[cfg(test)]
mod tests;

pub fn finish_operation(span: &Span, success: bool) {
    finish(
        span,
        if success {
            tonic::Code::Ok
        } else {
            tonic::Code::Unknown
        },
    );
}
