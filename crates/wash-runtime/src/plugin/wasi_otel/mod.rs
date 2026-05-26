mod bindings;
mod convert;
mod tracing;

use ::tracing::{info, warn};

use anyhow::{self, bail};
use dashmap::DashMap;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{SdkTracerProvider, Tracer as SdkTracer, TracerProviderBuilder};
use std::{collections::HashSet, sync::Arc, sync::OnceLock};

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};
use crate::engine::workload::WorkloadItem;
use crate::plugin::HostPlugin;
use crate::wit::{WitInterface, WitWorld};

use bindings::wasi::otel0_2_0_rc_3 as wasi_otel;

pub const WASI_OTEL_ID: &str = "wasi-otel";

/// Per-invocation tracing state
pub struct ComponentContext {
    component_name: String,
    workload_id: String,
    span_stack: Vec<opentelemetry::trace::SpanContext>,
    active_trace_id: Option<opentelemetry::TraceId>,
}

/// Per-invocation tracing state keyed by store ID.
#[derive(Default)]
pub struct WasiOtel {
    // Wait-free access after initialization
    pub provider: Arc<OnceLock<SdkTracerProvider>>,
    // Concurrent map for invocations without global blocking
    pub invocations: Arc<DashMap<String, ComponentContext>>,
    // Cache tracers to avoid re-creating scopes continuously
    pub tracers: Arc<DashMap<String, SdkTracer>>,
}

#[async_trait::async_trait]
impl HostPlugin for WasiOtel {
    fn id(&self) -> &'static str {
        WASI_OTEL_ID
    }

    fn world(&self) -> WitWorld {
        let interface = WitInterface::from("wasi:otel/types,tracing@0.2.0-rc.3");
        let imports = HashSet::from([interface]);

        WitWorld {
            imports,
            ..Default::default()
        }
    }

    async fn start(&self) -> anyhow::Result<()> {
        info!("Starting WASI OTel tracing plugin");

        let span_exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint("http://localhost:4317")
            .with_protocol(opentelemetry_otlp::Protocol::Grpc)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to create span exporter: {e}"))?;

        let resource = Resource::builder_empty()
            .with_attributes([opentelemetry::KeyValue::new("service.name", "wasi-otel")])
            .build();

        let provider = TracerProviderBuilder::default()
            .with_batch_exporter(span_exporter)
            .with_resource(resource)
            .build();

        let _ = self.provider.set(provider);

        Ok(())
    }

    async fn on_workload_item_bind<'a>(
        &self,
        comp_handle: &mut WorkloadItem<'a>,
        _: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        wasi_otel::types::add_to_linker::<_, SharedCtx>(comp_handle.linker(), extract_active_ctx)?;

        wasi_otel::tracing::add_to_linker::<_, SharedCtx>(
            comp_handle.linker(),
            extract_active_ctx,
        )?;

        let WorkloadItem::Component(comp_handle) = comp_handle else {
            bail!("Service can not be tracked");
        };

        info!(
            component_id = comp_handle.id(),
            component_name = %comp_handle.name(),
            "WASI OTel tracing interfaces bound to workload item"
        );

        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        _: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        self.invocations
            .retain(|_, state| state.workload_id != workload_id);

        info!(workload_id, "WASI OTel tracing unbound from workload");

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(provider) = self.provider.get() {
            if let Err(e) = provider.force_flush() {
                warn!("Failed to flush trace data during shutdown: {e}");
            }
            if let Err(e) = provider.shutdown() {
                warn!("Failed to shutdown tracer provider: {e}");
            }
        }

        info!("WASI OTel tracing stopped");
        Ok(())
    }
}

impl<'a> wasi_otel::types::Host for ActiveCtx<'a> {}
