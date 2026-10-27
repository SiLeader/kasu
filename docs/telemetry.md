# OpenTelemetry Tracing

English | [日本語](telemetry_ja.md)

`kasu-telemetry` exports `tracing` spans as OTLP/HTTP binary protobuf.
The CLI, daemon, cache, authoritative server, and Git ingest binaries share the same initialization.
By default nothing is exported and only stderr logging is initialized. `OTEL_TRACES_EXPORTER=otlp` enables export.
Metrics continue to come from the Prometheus endpoints of the [authoritative server](server-runtime.md) and the [cache / daemon](cache-hierarchy.md).

```sh
export OTEL_TRACES_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
```

Start each process with this environment. `/v1/traces` is appended to the generic endpoint,
so the example above exports to `http://127.0.0.1:4318/v1/traces`.
When `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is specified, that URL is used as is.
HTTPS uses WebPKI certificate verification. Start and operate the collector separately.

## Configuration

| Environment variable | Support |
|---|---|
| `OTEL_TRACES_EXPORTER` | `none` (default) or `otlp` |
| `OTEL_SDK_DISABLED` | `true` disables. Omitted or `false` uses the settings above |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Generic base URL. Defaults to `http://localhost:4318` |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Full trace-specific URL. Takes precedence over the generic setting |
| `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | Only `http/protobuf`. The trace-specific one takes precedence; same when omitted |
| `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | SDK header settings. Used for collector authentication and similar |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` (default), `parentbased_always_off`, `parentbased_traceidratio`, `always_on`, `always_off`, `traceidratio` |
| `OTEL_TRACES_SAMPLER_ARG` | Required for ratio samplers. Finite, 0–1 |
| `RUST_LOG` | Filter for stderr logs. Does not affect which spans OTLP exports or sampling |

Invalid settings when enabled are startup errors. Endpoints must be HTTP(S), at most 4096 bytes,
and URLs without userinfo, query, or fragment. Diagnostics never print configuration values.
When disabled, no exporter is created and export settings such as endpoints and samplers are not read.

`service.name` is the fixed value `kasu-cli`, `kasu-daemon`, `kasu-cache`, `kasu-server`, or `kasu-ingest`.
The resource also sets `service.version`. Overrides via `OTEL_SERVICE_NAME` / `OTEL_RESOURCE_ATTRIBUTES`,
`OTEL_PROPAGATORS`, and `OTEL_BSP_*` are not applied.
Compatibility with other common SDK settings beyond this table is not guaranteed.

The implementation locks OpenTelemetry 0.31 / OTLP 0.31.1 and tracing-opentelemetry 0.32.1,
compatible with the existing tonic / prost and Rust 1.90.
It uses the [official exporter API](https://docs.rs/opentelemetry-otlp/0.31.1/opentelemetry_otlp/) and
the [tracing bridge API](https://docs.rs/tracing-opentelemetry/0.32.1/tracing_opentelemetry/).

## Transport and Span Lifetimes

CLI commands have a root operation span and attach the current
W3C `traceparent` / `tracestate` to Metadata / Writer gRPC. Objects fetches have a client span covering the whole stream.
Over Unix sockets from daemon → cache → authority, the parent-child relationship is carried over for each adjacent RPC.
HTTP JSON and Git Smart HTTP accept the same W3C context.

The receiving side creates a new server span. A missing, invalid, duplicated, or over-512-byte traceparent
starts a new root. `traceparent` and `tracestate` are each limited to 512 bytes, and syntax validation
is done by the SDK's TraceContext propagator. Baggage is neither extracted nor forwarded.
Trace context is never used for authorization. The default ParentBased sampler respects the received sampled flag.

Authoritative server spans share the lifetime of the existing admission lease and do not close when response headers are generated.
They wait for the body to end or be dropped and for already started blocking workers to finish.
Cache / daemon server spans are also held by the response body and producer.
Spans and scoped subscribers are carried over into blocking workers and async tasks.

A cache miss has an `object_fill` operation span. With singleflight, one fill and its upstream RPC are attached to
the first request's trace, and each joining request has its own server span.
No fan-in links to other traces are added. Warm hits create no client span toward the upstream.

`kasu.code` is the numeric gRPC status. HTTP uses the same mapping as the [server metrics](server-runtime.md).
Normal completion is span status `OK`, and errors are `ERROR`. A body dropped midway maps to `CANCELLED`,
and body errors to `INTERNAL`. General CLI / ingest errors are `UNKNOWN`.
An HTTP 2xx Land business outcome is a success at the transport level. Ref conflicts are observed through the separate CAS metrics.

## Export Limits and Failures

The exporter uses a batch processor on a dedicated thread; application Tokio workers never do HTTP exports.

| Limit | Value |
|---|---:|
| Unexported span queue | 2048 |
| Per batch | 128 spans |
| Export interval | 1 second |
| Collector connection timeout | 1 second |
| Per HTTP export timeout | 2 seconds |
| Shutdown / flush wait at exit | Up to 3 seconds |
| Span attributes | Up to 16 |
| Span events / links | 0 |

When the queue is full, spans are dropped without delaying normal processing. Collector rejections, outages, and delays
never change the results of repository operations already completed. Short-lived CLI / ingest processes wait a bounded time to flush at exit.
Traces lost through long collector outages or queue overflow are not durably resent.
HTTP redirects are not followed, and collector credentials are never forwarded to another URL.
Normal exit via SIGINT flushes. Flushing on SIGKILL or process crashes is not guaranteed.

What OTLP exports is fixed spans on a dedicated target. Existing diagnostic events, Git pkt-lines,
bodies, paths, repos, Changeset / Blob IDs, endpoint URLs, and PATs are never captured as span attributes.
tracestate is handled as is, as W3C vendor context. The existing stderr diagnostic logs are a separate output governed by `RUST_LOG`.
OTLP metrics / logs, gRPC / JSON exporters, custom CAs, and dynamic configuration changes are not implemented.

## Library Use and Acceptance Tests

Embedding applications keep the guard from `kasu_telemetry::init` and drop it after closing every operation span.
When building your own subscriber, the published span / injection helpers and `GrpcLayer` can be used.
`http_router`, each `into_server()` of the authoritative server, and the cache / daemon `Service::into_server()` already include it.

```sh
cargo test -p kasu-telemetry --locked
cargo build -p kasu-server -p kasu-cli -p kasu-cache -p kasu-daemon -p kasu-ingest --locked
cargo build -p kasu-telemetry --example inspect_otlp --locked
python3 tools/telemetry/verify.py --out /tmp/kasu-telemetry
```

The runner uses real ingest and five real binaries, TCP gRPC / HTTP and Unix sockets, and an OTLP HTTP receiver.
OTLP requests are decoded with the `inspect_otlp` example using the official protobuf types, and it asserts trace IDs, every parent-child relationship and timing,
cold / warm paths, authentication failures, duplicated / invalid context, sampling, disabling, absence of secrets and baggage,
collector delays / redirects / outages, and normal exit.
Unit tests also verify scoped subscriber and blocking worker lifetimes and the bounded queue against a stuck exporter.

The [verification logs and hashes](simulations/telemetry/summary.json) store records for macOS arm64 and Linux arm64 with pinned Rust 1.90.
Linux can be rerun with `tools/telemetry/Dockerfile`. It has also been added to the CI configuration, but results on GitHub are unconfirmed.
Production collectors, long-term retention, large trace volumes, clock skew across hosts, and linking traces across durable webhooks are unverified future work.
