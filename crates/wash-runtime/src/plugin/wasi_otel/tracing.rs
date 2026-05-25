use std::sync::Arc;

use opentelemetry::trace::{
    Span as _, SpanContext as OtelSpanContext, TraceContextExt, TraceState, Tracer, TracerProvider,
};
use opentelemetry::{Context, InstrumentationScope, KeyValue, SpanId, TraceFlags, TraceId};
use tracing::{debug, warn};

use super::bindings::wasi::otel0_2_0_rc_3::tracing as wasi_tracing;
use super::convert::to_otel_attributes;
use super::{ComponentContext, WASI_OTEL_ID, WasiOtel};
use crate::engine::ctx::ActiveCtx;

impl<'a> wasi_tracing::Host for ActiveCtx<'a> {
    async fn on_start(&mut self, span_context: wasi_tracing::SpanContext) -> wasmtime::Result<()> {
        let component_id = Arc::clone(&self.ctx.component_id);

        debug!(
            component_id = %component_id,
            trace_id = %span_context.trace_id,
            span_id = %span_context.span_id,
            is_remote = span_context.is_remote,
            "guest span started"
        );

        let otel_ctx = OtelSpanContext::try_from(&span_context).unwrap_or_else(|_| {
            OtelSpanContext::new(
                TraceId::INVALID,
                SpanId::INVALID,
                TraceFlags::default(),
                span_context.is_remote,
                TraceState::default(),
            )
        });

        let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
            return Ok(());
        };

        let invocation_id = self.ctx.id.clone();
        let mut invocations = plugin.invocations.write().await;

        let state = invocations
            .entry(invocation_id)
            .or_insert_with(|| ComponentContext {
                component_name: component_id.to_string(),
                workload_id: self.ctx.workload_id.as_ref().to_string(),
                span_stack: Vec::new(),
                active_trace_id: None,
            });

        if state.span_stack.is_empty() {
            let host_trace_id = Context::current().span().span_context().trace_id();

            state.active_trace_id = Some(if host_trace_id != TraceId::INVALID {
                host_trace_id
            } else {
                TraceId::from(uuid::Uuid::new_v4().as_u128())
            });
        }

        state.span_stack.push(otel_ctx);
        debug!(component_id = %component_id, depth = state.span_stack.len(), "span stack depth after push");

        Ok(())
    }

    async fn on_end(&mut self, span_data: wasi_tracing::SpanData) -> wasmtime::Result<()> {
        let component_id = Arc::clone(&self.ctx.component_id);
        let invocation_id = self.ctx.id.clone();

        let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
            return Ok(());
        };

        let (component_name, trace_id) = {
            let mut invocations = plugin.invocations.write().await;

            let Some(state) = invocations.get_mut(&invocation_id) else {
                warn!(component_id = %component_id, "on_end: invocation not found");
                return Ok(());
            };

            state.span_stack.pop();

            let trace_id = state
                .active_trace_id
                .unwrap_or_else(|| TraceId::from(uuid::Uuid::new_v4().as_u128()));

            let component_name = state.component_name.clone();
            let should_remove = state.span_stack.is_empty();

            if !should_remove {
                state.active_trace_id = Some(trace_id);
            }

            if should_remove {
                let _ = invocations.remove(&invocation_id);
            }

            (component_name, trace_id)
        };

        let tracer = {
            let guard = plugin.provider.read().await;
            let Some(provider) = guard.as_ref() else {
                warn!(component_id = %component_id, "on_end: provider not initialized");
                return Ok(());
            };

            let scope = InstrumentationScope::builder(component_name.clone())
                .with_attributes([
                    KeyValue::new("service.name", component_name.clone()),
                    KeyValue::new("wasmcloud.component.id", component_id.clone()),
                ])
                .build();

            provider.tracer_with_scope(scope)
        };

        let mut builder = tracer
            .span_builder(span_data.name.clone())
            .with_kind(span_data.span_kind.into())
            .with_attributes(to_otel_attributes(&span_data.attributes))
            .with_start_time(&span_data.start_time)
            .with_trace_id(trace_id);

        let span_id: SpanId =
            SpanId::from_hex(&span_data.span_context.span_id).unwrap_or(SpanId::INVALID);

        if span_id != SpanId::INVALID {
            builder = builder.with_span_id(span_id);
        }

        let mut span =
            tracer.build_with_context(builder, &parent_context_for(&span_data, trace_id));

        for event in &span_data.events {
            span.add_event(event.name.clone(), to_otel_attributes(&event.attributes));
        }

        span.set_status((&span_data.status).into());
        span.end_with_timestamp((&span_data.end_time).into());

        debug!(component_id = %component_id, %trace_id, "span ended");

        Ok(())
    }

    async fn current_span_context(&mut self) -> wasmtime::Result<wasi_tracing::SpanContext> {
        let component_id = Arc::clone(&self.ctx.component_id);

        let (sc, source, stack_depth) = 'resolve: {
            let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
                break 'resolve (host_span_context(), "host_fallback", 0);
            };

            let invocations = plugin.invocations.read().await;

            let Some(state) = invocations.get(&self.ctx.id) else {
                break 'resolve (host_span_context(), "host_fallback", 0);
            };

            match state.span_stack.last().cloned() {
                Some(sc) => (sc, "guest", state.span_stack.len()),
                None => (host_span_context(), "host_fallback", 0),
            }
        };

        debug!(
            component_id = %component_id,
            trace_id = %sc.trace_id(),
            span_id = %sc.span_id(),
            stack_depth,
            source,
            "current_span_context"
        );

        Ok((&sc).into())
    }
}

fn host_span_context() -> OtelSpanContext {
    opentelemetry::Context::current()
        .span()
        .span_context()
        .clone()
}

fn parent_context_for(span_data: &wasi_tracing::SpanData, trace_id: TraceId) -> Context {
    let parent_span_id = SpanId::from_hex(&span_data.parent_span_id).unwrap_or(SpanId::INVALID);
    let parent_sc = OtelSpanContext::new(
        trace_id,
        parent_span_id,
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    );
    Context::current().with_remote_span_context(parent_sc)
}
