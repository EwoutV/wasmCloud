use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};

use crate::plugin::wasi_otel::bindings::wasi::clocks::wall_clock::Datetime as WitDatetime;

use super::bindings::wasi::otel0_2_0_rc_3 as wasi_otel;

use wasi_otel::tracing::{
    KeyValue as WitKeyValue, SpanContext as WitSpanContext, SpanKind as WitSpanKind,
    Status as WitStatus, TraceFlags as WitTraceFlags,
};

impl From<&SpanContext> for WitSpanContext {
    fn from(ctx: &SpanContext) -> Self {
        Self {
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
}

impl From<&WitSpanContext> for SpanContext {
    fn from(ctx: &WitSpanContext) -> Self {
        // Convert trace ID or take the outer trace ID. If none, generate one.
        let trace_id = TraceId::from_hex(&ctx.trace_id)
            .ok()
            .filter(|trace_id| *trace_id != TraceId::INVALID)
            .unwrap_or_else(|| {
                let host_trace_id = opentelemetry::Context::current()
                    .span()
                    .span_context()
                    .trace_id();

                if host_trace_id == TraceId::INVALID {
                    TraceId::from(rand::random::<u128>())
                } else {
                    host_trace_id
                }
            });

        // Convert span ID or generate one.
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
}

impl From<WitSpanKind> for SpanKind {
    fn from(kind: WitSpanKind) -> Self {
        match kind {
            WitSpanKind::Client => SpanKind::Client,
            WitSpanKind::Server => SpanKind::Server,
            WitSpanKind::Producer => SpanKind::Producer,
            WitSpanKind::Consumer => SpanKind::Consumer,
            WitSpanKind::Internal => SpanKind::Internal,
        }
    }
}

impl From<&WitStatus> for Status {
    fn from(status: &WitStatus) -> Self {
        match status {
            WitStatus::Unset => Status::Unset,
            WitStatus::Ok => Status::Ok,
            WitStatus::Error(description) => Status::error(description.clone()),
        }
    }
}

impl From<&WitDatetime> for SystemTime {
    fn from(dt: &super::bindings::wasi::clocks::wall_clock::Datetime) -> Self {
        UNIX_EPOCH + Duration::new(dt.seconds, dt.nanoseconds)
    }
}

pub(super) fn to_otel_attributes(attrs: &[WitKeyValue]) -> Vec<KeyValue> {
    attrs
        .iter()
        .map(|kv| KeyValue::new(kv.key.clone(), kv.value.clone()))
        .collect()
}
