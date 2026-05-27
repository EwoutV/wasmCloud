use std::sync::Arc;

use opentelemetry::trace::{
    Span as _, SpanContext as OtelSpanContext, TraceContextExt, TraceFlags, TraceState, Tracer,
    TracerProvider,
};
use opentelemetry::{Context, InstrumentationScope, KeyValue, SpanId, TraceId};
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

        let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
            return Ok(());
        };

        let invocation_id = self.ctx.id.clone();

        let mut state = plugin.invocations.entry(invocation_id).or_insert_with(|| {
            ComponentContext::new(
                component_id.to_string(),
                self.ctx.workload_id.as_ref().to_string(),
            )
        });

        let otel_ctx = OtelSpanContext::from(&span_context);

        state.span_stack.push(otel_ctx);

        Ok(())
    }

    async fn on_end(&mut self, span_data: wasi_tracing::SpanData) -> wasmtime::Result<()> {
        let component_id = Arc::clone(&self.ctx.component_id);
        let invocation_id = self.ctx.id.clone();

        let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
            return Ok(());
        };

        let (component_name, trace_id, should_remove) = {
            let Some(mut state) = plugin.invocations.get_mut(&invocation_id) else {
                warn!(component_id = %component_id, "on_end: invocation not found");
                return Ok(());
            };

            let Ok(span_id) = SpanId::from_hex(&span_data.span_context.span_id) else {
                warn!(component_id = %component_id, "on_end: invalid span id");
                return Ok(());
            };

            let Some(index) = state
                .span_stack
                .iter()
                .rposition(|span_context| span_context.span_id() == span_id)
            else {
                warn!(component_id = %component_id, %span_id, "on_end: span not found in stack");
                return Ok(());
            };

            let span_context = state.span_stack.remove(index);

            let trace_id = span_context.trace_id();

            let component_name = state.component_name.clone();
            let should_remove = state.span_stack.is_empty();

            (component_name, trace_id, should_remove)
        };

        if should_remove {
            plugin.invocations.remove(&invocation_id);
        }

        let Some(provider) = plugin.provider.get() else {
            warn!(component_id = %component_id, "on_end: tracer provider not initialized");
            return Ok(());
        };

        // Retrieve or create the Tracer cache
        let tracer = plugin
            .tracers
            .entry(component_name.clone())
            .or_insert_with(|| {
                let scope = InstrumentationScope::builder(component_name.clone())
                    .with_attributes([
                        KeyValue::new("service.name", component_name.clone()),
                        KeyValue::new("wasmcloud.component.id", component_id.clone()),
                    ])
                    .build();
                provider.tracer_with_scope(scope)
            });

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

        let mut span = if let Some(parent_context) = parent_context_for(&span_data, trace_id) {
            tracer.build_with_context(builder, &parent_context)
        } else {
            tracer.build(builder)
        };

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

        let sc = 'resolve: {
            let host_span_ctx = Context::current().span().span_context().clone();

            let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
                break 'resolve host_span_ctx;
            };

            let Some(state) = plugin.invocations.get(&self.ctx.id) else {
                break 'resolve host_span_ctx;
            };

            match state.span_stack.last().cloned() {
                Some(sc) => sc,
                None => host_span_ctx,
            }
        };

        debug!(
            component_id = %component_id,
            trace_id = %sc.trace_id(),
            span_id = %sc.span_id(),
            "current_span_context"
        );

        Ok((&sc).into())
    }
}

fn parent_context_for(span_data: &wasi_tracing::SpanData, trace_id: TraceId) -> Option<Context> {
    let parent_span_id = SpanId::from_hex(&span_data.parent_span_id).unwrap_or(SpanId::INVALID);

    if parent_span_id == SpanId::INVALID {
        return None;
    }

    let parent_sc = OtelSpanContext::new(
        trace_id,
        parent_span_id,
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    );

    Some(Context::current().with_remote_span_context(parent_sc))
}
