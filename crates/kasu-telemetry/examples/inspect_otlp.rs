//! Decode an OTLP trace request from stdin for the acceptance runner.
use opentelemetry_proto::tonic::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{AnyValue, KeyValue, any_value::Value},
};
use prost::Message;
use serde_json::{Value as Json, json};
use std::io::Read;
fn value(value: Option<AnyValue>) -> Json {
    match value.and_then(|v| v.value) {
        Some(Value::StringValue(v)) => json!(v),
        Some(Value::BoolValue(v)) => json!(v),
        Some(Value::IntValue(v)) => json!(v),
        Some(Value::DoubleValue(v)) => json!(v),
        _ => Json::Null,
    }
}
fn attributes(values: Vec<KeyValue>) -> serde_json::Map<String, Json> {
    values
        .into_iter()
        .map(|kv| (kv.key, value(kv.value)))
        .collect()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut raw = vec![];
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > 16 * 1024 * 1024 {
        return Err("OTLP batch too large".into());
    }
    let request = ExportTraceServiceRequest::decode(raw.as_slice())?;
    let mut spans = vec![];
    for resource in request.resource_spans {
        let attrs = attributes(resource.resource.map(|r| r.attributes).unwrap_or_default());
        for scope in resource.scope_spans {
            for span in scope.spans {
                spans.push(json!({"resource":attrs,"trace_id":hex(&span.trace_id),"span_id":hex(&span.span_id),"parent_span_id":hex(&span.parent_span_id),"name":span.name,"kind":span.kind,"trace_state":span.trace_state,"flags":span.flags,"start_nanos":span.start_time_unix_nano,"end_nanos":span.end_time_unix_nano,"attributes":attributes(span.attributes),"events":span.events.len(),"links":span.links.len(),"status":span.status.map(|s|s.code)}));
            }
        }
    }
    println!("{}", serde_json::to_string(&spans)?);
    Ok(())
}
