//! OpenTelemetry wiring for the benchmark binary.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{MeterProviderBuilder, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler, SdkTracerProvider};
use opentelemetry_semantic_conventions::SCHEMA_URL;
use opentelemetry_semantic_conventions::attribute::SERVICE_VERSION;
use tracing_opentelemetry::{MetricsLayer, OpenTelemetryLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[derive(Default)]
pub struct Guard {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(tracer) = &self.tracer {
            let _ = tracer.force_flush();
            let _ = tracer.shutdown();
        }
        if let Some(logger) = &self.logger {
            let _ = logger.force_flush();
            let _ = logger.shutdown();
        }
        if let Some(meter) = &self.meter {
            let _ = meter.shutdown();
        }
    }
}

fn console_filter() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| {
        "info,pay_core=error,pay_kit::mpp=warn,hyper=warn,reqwest=warn,tower=warn".to_string()
    })
}

fn trace_filter() -> String {
    std::env::var("BENCH_TRACE_FILTER").unwrap_or_else(|_| {
        "info,hyper=warn,reqwest=warn,tower=warn,h2=warn,opentelemetry=warn".to_string()
    })
}

pub fn init(service_name: &str, otlp: Option<&str>) -> Guard {
    global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());
    let Some(endpoint) = otlp else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(console_filter()))
            .with_target(false)
            .with_thread_names(true)
            .try_init();
        return Guard::default();
    };

    let base = normalize_base(endpoint);
    let resource = resource(service_name);
    let tracer = tracer_provider(&format!("{base}/v1/traces"), resource.clone())
        .inspect_err(|error| eprintln!("OTLP trace init failed: {error}"))
        .ok();
    let meter = meter_provider(&format!("{base}/v1/metrics"), resource.clone())
        .inspect_err(|error| eprintln!("OTLP metric init failed: {error}"))
        .ok();
    let logger = logger_provider(&format!("{base}/v1/logs"), resource)
        .inspect_err(|error| eprintln!("OTLP log init failed: {error}"))
        .ok();
    let trace_layer = tracer.as_ref().map(|provider| {
        OpenTelemetryLayer::new(provider.tracer(service_name.to_string()))
            .with_filter(EnvFilter::new(trace_filter()))
    });
    let metrics_layer = meter.as_ref().map(|provider| {
        MetricsLayer::new(provider.clone()).with_filter(EnvFilter::new(trace_filter()))
    });
    let logs_layer = logger.as_ref().map(|provider| {
        OpenTelemetryTracingBridge::new(provider).with_filter(EnvFilter::new(trace_filter()))
    });
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_names(true)
                .with_filter(EnvFilter::new(console_filter())),
        )
        .with(trace_layer)
        .with(metrics_layer)
        .with(logs_layer)
        .try_init();
    Guard {
        tracer,
        meter,
        logger,
    }
}

fn normalize_base(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    }
}

fn resource(service_name: &str) -> Resource {
    Resource::builder()
        .with_service_name(service_name.to_string())
        .with_schema_url(
            [KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION"))],
            SCHEMA_URL,
        )
        .build()
}

fn tracer_provider(endpoint: &str, resource: Resource) -> Result<SdkTracerProvider, String> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|error| format!("OTLP span exporter: {error}"))?;
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            1.0,
        ))))
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();
    global::set_tracer_provider(provider.clone());
    Ok(provider)
}

fn logger_provider(endpoint: &str, resource: Resource) -> Result<SdkLoggerProvider, String> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|error| format!("OTLP log exporter: {error}"))?;
    Ok(SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

fn meter_provider(endpoint: &str, resource: Resource) -> Result<SdkMeterProvider, String> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|error| format!("OTLP metric exporter: {error}"))?;
    let reader = PeriodicReader::builder(exporter)
        .with_interval(Duration::from_secs(15))
        .build();
    let provider = MeterProviderBuilder::default()
        .with_resource(resource)
        .with_reader(reader)
        .build();
    global::set_meter_provider(provider.clone());
    Ok(provider)
}

pub fn named_runtime(prefix: &'static str) -> std::io::Result<tokio::runtime::Runtime> {
    let counter = AtomicUsize::new(0);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name_fn(move || {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            format!("{prefix}-worker-{n}")
        })
        .build()
}
