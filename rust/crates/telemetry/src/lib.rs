//! Shared, bounded-cardinality telemetry. Never record credentials or request bodies.
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use http::HeaderMap;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{TraceContextExt, TracerProvider};
use opentelemetry_sdk::{Resource, propagation::TraceContextPropagator, trace::SdkTracerProvider};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub struct Telemetry(SdkTracerProvider);

impl Drop for Telemetry {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

/// Run exporter construction outside Tokio: the blocking HTTP exporter owns its runtime.
pub fn init(service: &'static str) -> Result<Telemetry> {
    // kube-rs and the OTLP HTTP exporter may enable different Rustls backends.
    // Select one explicitly rather than relying on ambiguous feature inference.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    std::thread::spawn(move || {
        let mut builder = SdkTracerProvider::builder()
            .with_resource(Resource::builder().with_service_name(service).build());
        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
            || std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some()
        {
            use opentelemetry_otlp::WithExportConfig;
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_timeout(Duration::from_secs(3))
                .build()?;
            builder = builder.with_batch_exporter(exporter);
        }
        // SDK honours OTEL_TRACES_SAMPLER / ARG. Queue is bounded by the SDK;
        // observability outages must never become inference admission failures.
        let provider = builder.build();
        let tracer = provider.tracer("xscope");
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer));
        if std::env::var("XSCOPE_LOG_FORMAT").as_deref() == Ok("json") {
            subscriber
                .with(tracing_subscriber::fmt::layer().json())
                .try_init()?;
        } else {
            subscriber
                .with(tracing_subscriber::fmt::layer().compact())
                .try_init()?;
        }
        start_metrics_server()?;
        Ok(Telemetry(provider))
    })
    .join()
    .map_err(|_| anyhow!("telemetry initialization thread panicked"))?
}

fn start_metrics_server() -> Result<()> {
    let Ok(address) = std::env::var("XSCOPE_METRICS_ADDRESS") else {
        return Ok(());
    };
    let listener = std::net::TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    std::thread::Builder::new()
        .name("metrics".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "metrics runtime could not be created");
                    return;
                }
            };
            runtime.block_on(async {
                let router = axum::Router::new().route(
                    "/metrics",
                    axum::routing::get(|| async {
                        (
                            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
                            metrics_text(),
                        )
                    }),
                );
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::error!(%error, "metrics socket could not join Tokio runtime");
                        return;
                    }
                };
                if let Err(error) = axum::serve(listener, router).await {
                    tracing::error!(%error, "metrics listener stopped");
                }
            });
        })
        .context("spawn metrics server thread")?;
    Ok(())
}

struct Headers<'a>(&'a HeaderMap);
impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.to_str().ok()
    }
    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .map(http::header::HeaderName::as_str)
            .collect()
    }
}
struct HeaderWriter<'a>(&'a mut HeaderMap);
impl Injector for HeaderWriter<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(key.as_bytes()),
            value.parse(),
        ) {
            self.0.insert(name, value);
        }
    }
}

/// Only W3C trace context, never baggage or arbitrary user headers.
pub fn inject(span: &Span, headers: &mut HeaderMap) {
    headers.remove("traceparent");
    headers.remove("tracestate");
    headers.remove("baggage");
    TraceContextPropagator::new().inject_context(&span.context(), &mut HeaderWriter(headers));
}

pub struct RequestTrace {
    pub span: Span,
    route: String,
    method: &'static str,
    started: Instant,
    finished: bool,
    first_byte: bool,
}

impl RequestTrace {
    /// `route` must be a server-owned route template, NEVER the untrusted URI.
    pub fn start(headers: &HeaderMap, method: &str, route: &str) -> Self {
        let method = match method {
            "GET" => "GET",
            "POST" => "POST",
            "PUT" => "PUT",
            "DELETE" => "DELETE",
            "PATCH" => "PATCH",
            _ => "OTHER",
        };
        let span = tracing::info_span!(
            "http.request",
            otel.kind = "server",
            http.request.method = method,
            http.route = route,
            http.response.status_code = tracing::field::Empty,
            xscope.outcome = tracing::field::Empty,
            xscope.route.pool = tracing::field::Empty,
            xscope.route.revision = tracing::field::Empty,
            xscope.model.revision = tracing::field::Empty,
            trace_id = tracing::field::Empty
        );
        let parent = TraceContextPropagator::new().extract(&Headers(headers));
        let _ = span.set_parent(parent);
        let trace_id = span.context().span().span_context().trace_id().to_string();
        span.record("trace_id", &trace_id);
        METRICS.inflight.with_label_values(&[route]).inc();
        Self {
            span,
            route: route.into(),
            method,
            started: Instant::now(),
            finished: false,
            first_byte: false,
        }
    }

    pub fn trace_id(&self) -> String {
        self.span
            .context()
            .span()
            .span_context()
            .trace_id()
            .to_string()
    }

    pub fn selected_pool(&self, pool: &str, model_revision: &str, policy_revision: i64) {
        self.span.record("xscope.route.pool", pool);
        self.span.record("xscope.route.revision", policy_revision);
        self.span.record("xscope.model.revision", model_revision);
    }

    /// Time to first nonempty upstream body byte, not necessarily first model token.
    pub fn observe_first_byte(&mut self) {
        if !self.first_byte {
            self.first_byte = true;
            METRICS
                .first_byte
                .with_label_values(&[&self.route])
                .observe(self.started.elapsed().as_secs_f64());
        }
    }

    pub fn finish(&mut self, status: u16, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let status = status.to_string();
        METRICS
            .requests
            .with_label_values(&[self.route.as_str(), self.method, status.as_str(), outcome])
            .inc();
        METRICS
            .duration
            .with_label_values(&[self.route.as_str(), self.method])
            .observe(self.started.elapsed().as_secs_f64());
        METRICS.inflight.with_label_values(&[&self.route]).dec();
        self.span.record("http.response.status_code", &status);
        self.span.record("xscope.outcome", outcome);
        if status.starts_with('5') || outcome == "provider_error" {
            self.span
                .set_status(opentelemetry::trace::Status::error(outcome));
        }
        tracing::info!(parent: &self.span, elapsed_seconds = self.started.elapsed().as_secs_f64(), outcome, "request completed");
    }
}

impl Drop for RequestTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(499, "cancelled");
        }
    }
}

struct Metrics {
    registry: Registry,
    requests: IntCounterVec,
    inflight: IntGaugeVec,
    duration: HistogramVec,
    first_byte: HistogramVec,
    pub events: IntCounterVec,
    tokens: IntCounterVec,
    routes: IntCounterVec,
    wal_storage: IntGaugeVec,
}
// Metric names, help strings and label sets are compile-time constants; each
// descriptor is unique in this private registry, so construction/registration cannot fail.
#[allow(clippy::unwrap_used)]
static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let registry = Registry::new();
    let requests = IntCounterVec::new(
        Opts::new(
            "xscope_http_requests_total",
            "Completed HTTP requests, including rejection and cancellation",
        ),
        &["route", "method", "status", "outcome"],
    )
    .unwrap();
    let inflight = IntGaugeVec::new(
        Opts::new("xscope_http_inflight", "Active HTTP requests"),
        &["route"],
    )
    .unwrap();
    let duration = HistogramVec::new(
        HistogramOpts::new(
            "xscope_http_duration_seconds",
            "Full request lifetime including SSE",
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.5, 1., 5., 15., 60., 300.]),
        &["route", "method"],
    )
    .unwrap();
    let first_byte = HistogramVec::new(
        HistogramOpts::new(
            "xscope_http_first_byte_seconds",
            "Time to first nonempty upstream body byte",
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.5, 1., 5., 15., 60.]),
        &["route"],
    )
    .unwrap();
    let events = IntCounterVec::new(
        Opts::new("xscope_background_events_total", "WAL and quota outcomes"),
        &["operation", "outcome"],
    )
    .unwrap();
    let tokens = IntCounterVec::new(
        Opts::new(
            "xscope_inference_tokens_total",
            "Provider-reported tokens only; unknown usage is not estimated",
        ),
        &["direction"],
    )
    .unwrap();
    let routes = IntCounterVec::new(
        Opts::new(
            "xscope_route_selections_total",
            "Pool selections before body admission; not completed requests",
        ),
        &["pool", "reason"],
    )
    .unwrap();
    let wal_storage = IntGaugeVec::new(
        Opts::new(
            "xscope_wal_storage_bytes",
            "Gateway retained data, in-flight space reservations and filesystem watermarks",
        ),
        &["kind"],
    )
    .unwrap();
    for collector in [
        Box::new(wal_storage.clone()) as Box<dyn prometheus::core::Collector>,
        Box::new(requests.clone()) as Box<dyn prometheus::core::Collector>,
        Box::new(inflight.clone()),
        Box::new(duration.clone()),
        Box::new(first_byte.clone()),
        Box::new(events.clone()),
        Box::new(tokens.clone()),
        Box::new(routes.clone()),
    ] {
        registry.register(collector).unwrap();
    }
    Metrics {
        wal_storage,
        registry,
        requests,
        inflight,
        duration,
        first_byte,
        events,
        tokens,
        routes,
    }
});

pub fn background_event(operation: &'static str, outcome: &'static str) {
    METRICS
        .events
        .with_label_values(&[operation, outcome])
        .inc();
}

/// Bounded labels, sampled when Gateway checks admission/readiness.
pub fn wal_storage(retained: u64, reserved: u64, free: u64, limit: u64, floor: u64) {
    for (kind, value) in [
        ("retained", retained),
        ("reserved", reserved),
        ("free", free),
        ("limit", limit),
        ("free_floor", floor),
    ] {
        METRICS
            .wal_storage
            .with_label_values(&[kind])
            .set(i64::try_from(value).unwrap_or(i64::MAX));
    }
}
pub fn tokens(input: u64, output: u64) {
    METRICS.tokens.with_label_values(&["input"]).inc_by(input);
    METRICS.tokens.with_label_values(&["output"]).inc_by(output);
}

/// Only call with statically registered pool IDs and bounded reasons.
pub fn route_selected(pool: &str, reason: &'static str) {
    METRICS.routes.with_label_values(&[pool, reason]).inc();
}
pub fn metrics_text() -> String {
    let mut output = Vec::new();
    if let Err(error) =
        prometheus::TextEncoder::new().encode(&METRICS.registry.gather(), &mut output)
    {
        tracing::error!(%error, "metrics encoding failed");
        return String::new();
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn startup_selects_tls_provider() {
        let _telemetry = init("xscope-telemetry-test").unwrap();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
    #[test]
    fn completion_is_exactly_once_and_drop_releases_inflight() {
        let mut request = RequestTrace::start(&HeaderMap::new(), "POST", "/unit/completion");
        request.finish(403, "rejected");
        request.finish(200, "succeeded");
        drop(request);
        assert_eq!(
            METRICS
                .requests
                .with_label_values(&["/unit/completion", "POST", "403", "rejected"])
                .get(),
            1
        );
        assert_eq!(
            METRICS
                .inflight
                .with_label_values(&["/unit/completion"])
                .get(),
            0
        );
        drop(RequestTrace::start(
            &HeaderMap::new(),
            "SECRET_METHOD",
            "/unit/drop",
        ));
        assert_eq!(
            METRICS
                .requests
                .with_label_values(&["/unit/drop", "OTHER", "499", "cancelled"])
                .get(),
            1
        );
    }
    #[test]
    fn propagation_rejects_invalid_context_and_never_injects_baggage() {
        let headers = HeaderMap::from_iter([(
            "traceparent".parse().unwrap(),
            "not-a-trace".parse().unwrap(),
        )]);
        let context = TraceContextPropagator::new().extract(&Headers(&headers));
        assert!(!context.span().span_context().is_valid());
        let mut output =
            HeaderMap::from_iter([("baggage".parse().unwrap(), "secret=token".parse().unwrap())]);
        inject(&Span::none(), &mut output);
        assert!(!output.contains_key("baggage"));
    }
}
