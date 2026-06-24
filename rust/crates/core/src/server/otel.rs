//! Distributed-tracing context propagation (feature `otel`).
//!
//! Extracts a W3C `traceparent` from an incoming request's headers and parents
//! the current server span to it, so a client's trace (e.g. pay-bench) and this
//! proxy's spans stitch into one end-to-end waterfall. The binary is
//! responsible for installing a text-map propagator
//! (`opentelemetry::global::set_text_map_propagator`).

use opentelemetry::propagation::Extractor;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Adapts an `axum::http::HeaderMap` to the OpenTelemetry [`Extractor`] interface.
struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Parent `span` to the trace context carried in `headers` (if any), so this
/// request's spans stitch onto the caller's trace.
///
/// Must be called **before** `span` is entered/instrumented: tracing-opentelemetry
/// fixes a span's trace id from its parent at creation/entry, so setting the
/// parent afterwards would leave it a root. No-op when no propagator is
/// installed or no `traceparent` is present — the span simply stays a root.
pub fn set_parent_from_headers(span: &tracing::Span, headers: &axum::http::HeaderMap) {
    let parent = opentelemetry::global::get_text_map_propagator(|prop| {
        prop.extract(&HeaderExtractor(headers))
    });
    let _ = span.set_parent(parent);
}
