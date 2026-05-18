use std::sync::Arc;

use opentelemetry::trace::{
    Span as _, SpanContext as OtelSpanContext, TraceContextExt, TraceState, Tracer, TracerProvider,
};
use opentelemetry::{Context, InstrumentationScope, KeyValue, SpanId, TraceFlags, TraceId};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::bindings::wasi::otel0_2_0_rc_3::tracing as wasi_tracing;
use super::convert::{
    to_native_systime, to_otel_attributes, to_otel_span_context, to_otel_span_kind, to_otel_status,
    to_wit_span_context,
};
use super::{WASI_OTEL_ID, WasiOtel};

use crate::engine::ctx::ActiveCtx;

impl<'a> wasi_tracing::Host for ActiveCtx<'a> {
    async fn on_start(&mut self, span_context: wasi_tracing::SpanContext) -> wasmtime::Result<()> {
        let otel_ctx = to_otel_span_context(&span_context);
        let component_id = Arc::clone(&self.ctx.component_id);

        info!(
            component_id = %component_id,
            trace_id = %span_context.trace_id,
            span_id = %span_context.span_id,
            is_remote = span_context.is_remote,
            "guest span started"
        );

        if let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) {
            let mut tracker = plugin.tracker.write().await;

            if let Some(comp_ctx) = tracker.get_component_data_mut(component_id.as_ref()) {
                if comp_ctx.span_stack.is_empty() {
                    // Prefer the host's active trace ID so guest spans are stitched
                    // into the same trace when host OTel is enabled. Fall back to a
                    // fresh UUID when the host has no active span.
                    let host_trace_id = Context::current().span().span_context().trace_id();

                    comp_ctx.active_trace_id = Some(if host_trace_id != TraceId::INVALID {
                        host_trace_id
                    } else {
                        TraceId::from(Uuid::new_v4().as_u128())
                    });
                }

                comp_ctx.span_stack.push(otel_ctx);
                debug!(component_id = %component_id, depth = comp_ctx.span_stack.len(), "span stack depth after push");
            } else {
                warn!(component_id = %component_id, "on_start: component not found in tracker");
            }
        }

        Ok(())
    }

    async fn on_end(&mut self, span_data: wasi_tracing::SpanData) -> wasmtime::Result<()> {
        let component_id = Arc::clone(&self.ctx.component_id);

        let Some(plugin) = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID) else {
            return Ok(());
        };

        // Clone component_name while holding the tracker lock, then release before
        // acquiring the provider lock — avoids any potential lock ordering issues.
        let (component_name, trace_id) = {
            let mut tracker = plugin.tracker.write().await;

            let Some(comp_ctx) = tracker.get_component_data_mut(component_id.as_ref()) else {
                warn!(component_id = %component_id, "on_end: component not found in tracker");
                return Ok(());
            };

            comp_ctx.span_stack.pop();
            debug!(component_id = %component_id, depth = comp_ctx.span_stack.len(), "span stack depth after pop");

            let trace_id = comp_ctx
                .active_trace_id
                .unwrap_or_else(|| TraceId::from(Uuid::new_v4().as_u128()));

            if comp_ctx.span_stack.is_empty() {
                comp_ctx.active_trace_id = None;
            }

            (comp_ctx.component_name.clone(), trace_id)
        };

        info!("Ending with trace ID {trace_id}");

        // Get a tracer from the shared provider, scoped to this component.
        let tracer = {
            let guard = plugin.provider.read().await;

            let Some(p) = guard.as_ref() else {
                warn!(component_id = %component_id, "on_end: provider not initialized");
                return Ok(());
            };

            let scope = InstrumentationScope::builder(component_name.clone())
                .with_attributes([
                    KeyValue::new("service.name", component_name.clone()),
                    KeyValue::new("wasmcloud.component.id", component_id.clone()),
                ])
                .build();

            p.tracer_with_scope(scope)
        };

        let span_id = SpanId::from_hex(&span_data.span_context.span_id).unwrap_or(SpanId::INVALID);

        // Attach identity attributes so every span is queryable by component and
        // service name regardless of which instrumentation scope it was emitted from.
        let attrs = to_otel_attributes(&span_data.attributes);

        let mut builder = tracer
            .span_builder(span_data.name.clone())
            .with_kind(to_otel_span_kind(span_data.span_kind))
            .with_attributes(attrs)
            .with_start_time(to_native_systime(&span_data.start_time))
            .with_trace_id(trace_id);

        if span_id != SpanId::INVALID {
            builder = builder.with_span_id(span_id);
        }

        let parent_cx = parent_context_for(&span_data, trace_id);
        let mut span = tracer.build_with_context(builder, &parent_cx);

        for event in &span_data.events {
            span.add_event(event.name.clone(), to_otel_attributes(&event.attributes));
        }

        span.set_status(to_otel_status(&span_data.status));
        span.end_with_timestamp(to_native_systime(&span_data.end_time));

        Ok(())
    }

    async fn current_span_context(&mut self) -> wasmtime::Result<wasi_tracing::SpanContext> {
        let component_id = Arc::clone(&self.ctx.component_id);
        let plugin = self.ctx.get_plugin::<WasiOtel>(WASI_OTEL_ID);

        if let Some(plugin) = plugin {
            let tracker = plugin.tracker.read().await;

            if let Some(comp_ctx) = tracker.get_component_data(component_id.as_ref()) {
                if let Some(sc) = comp_ctx.span_stack.last() {
                    info!(
                        component_id = %component_id,
                        trace_id = %sc.trace_id(),
                        span_id = %sc.span_id(),
                        stack_depth = comp_ctx.span_stack.len(),
                        source = "guest",
                        "current_span_context"
                    );
                    return Ok(to_wit_span_context(sc));
                }
            }
        }

        // No active guest span — fall back to the host's current span.
        let sc = opentelemetry::Context::current()
            .span()
            .span_context()
            .clone();

        info!(
            component_id = %component_id,
            trace_id = %sc.trace_id(),
            span_id = %sc.span_id(),
            is_valid = sc.is_valid(),
            source = "host_fallback",
            "current_span_context"
        );

        Ok(to_wit_span_context(&sc))
    }
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
