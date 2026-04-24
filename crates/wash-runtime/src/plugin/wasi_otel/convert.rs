use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};

use super::bindings::wasi::otel0_3_0 as wasi_otel;

use wasi_otel::tracing::{
    KeyValue as WitKeyValue, SpanContext as WitSpanContext, SpanKind as WitSpanKind,
    Status as WitStatus, TraceFlags as WitTraceFlags,
};

pub(super) fn to_wit_span_context(ctx: &SpanContext) -> WitSpanContext {
    WitSpanContext {
        trace_id: format!("{:032x}", ctx.trace_id()),
        span_id: format!("{:016x}", ctx.span_id()),
        trace_flags: if ctx.is_sampled() {
            WitTraceFlags::SAMPLED
        } else {
            WitTraceFlags::empty()
        },
        is_remote: ctx.is_remote(),
        trace_state: vec![],
    }
}

pub(super) fn to_otel_span_context(ctx: &WitSpanContext) -> SpanContext {
    let trace_id = TraceId::from_hex(&ctx.trace_id).unwrap_or(TraceId::INVALID);
    let span_id = SpanId::from_hex(&ctx.span_id).unwrap_or(SpanId::INVALID);

    let trace_flags = if ctx.trace_flags.contains(WitTraceFlags::SAMPLED) {
        TraceFlags::SAMPLED
    } else {
        TraceFlags::default()
    };

    let trace_state = TraceState::from_key_value(
        ctx.trace_state
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    )
    .unwrap_or_default();

    SpanContext::new(trace_id, span_id, trace_flags, ctx.is_remote, trace_state)
}

pub(super) fn to_otel_span_kind(kind: WitSpanKind) -> SpanKind {
    match kind {
        WitSpanKind::Client => SpanKind::Client,
        WitSpanKind::Server => SpanKind::Server,
        WitSpanKind::Producer => SpanKind::Producer,
        WitSpanKind::Consumer => SpanKind::Consumer,
        WitSpanKind::Internal => SpanKind::Internal,
    }
}

pub(super) fn to_otel_status(status: &WitStatus) -> Status {
    match status {
        WitStatus::Unset => Status::Unset,
        WitStatus::Ok => Status::Ok,
        WitStatus::Error(description) => Status::error(description.clone()),
    }
}

pub(super) fn to_otel_attributes(attrs: &[WitKeyValue]) -> Vec<KeyValue> {
    attrs
        .iter()
        .map(|kv| KeyValue::new(kv.key.clone(), kv.value.clone()))
        .collect()
}

pub(super) fn to_native_systime(
    dt: &super::bindings::wasi::clocks::wall_clock::Datetime,
) -> SystemTime {
    UNIX_EPOCH + Duration::new(dt.seconds, dt.nanoseconds)
}
