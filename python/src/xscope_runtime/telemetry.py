"""ASGI lifecycle telemetry: the span covers the entire SSE response and cancellation."""
import asyncio
import atexit
import logging
import os
import time
from functools import cache

from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.trace.propagation.tracecontext import TraceContextTextMapPropagator
from prometheus_client import CollectorRegistry, Counter, Gauge, Histogram, start_http_server

REGISTRY = CollectorRegistry()
REQUESTS = Counter("xscope_runtime_requests_total", "Completed runtime requests", ["route", "status", "outcome"], registry=REGISTRY)
INFLIGHT = Gauge("xscope_runtime_inflight", "Active inference requests", registry=REGISTRY)
DURATION = Histogram("xscope_runtime_duration_seconds", "Full response lifetime", ["route"], buckets=(.01, .05, .1, .5, 1, 5, 15, 60, 300), registry=REGISTRY)
LOGGER = logging.getLogger("uvicorn.error")


@cache
def configure():
    provider = TracerProvider(resource=Resource.create({"service.name": "xscope-runtime"}))
    if os.getenv("OTEL_EXPORTER_OTLP_ENDPOINT") or os.getenv("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"):
        provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter(timeout=3), max_queue_size=1024, max_export_batch_size=128))
    atexit.register(provider.shutdown)
    if address := os.getenv("XSCOPE_METRICS_ADDRESS"):
        host, port = address.rsplit(":", 1)
        start_http_server(int(port), addr=host, registry=REGISTRY)
    return provider.get_tracer("xscope.runtime")


class TelemetryMiddleware:
    def __init__(self, app, tracer):
        self.app = app
        self.tracer = tracer

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or scope["path"] in ("/healthz", "/readyz"):
            return await self.app(scope, receive, send)
        route = "/v1/chat/completions" if scope["path"] == "/v1/chat/completions" else "unmatched"
        # Extract only trace context: never record prompts, query strings or credentials.
        carrier = {k.decode("latin1"): v.decode("latin1") for k, v in scope["headers"] if k in (b"traceparent", b"tracestate")}
        parent = TraceContextTextMapPropagator().extract(carrier)
        started = time.monotonic()
        status, completed, outcome = 500, False, "provider_error"
        INFLIGHT.inc()
        with self.tracer.start_as_current_span("runtime.inference", context=parent, kind=trace.SpanKind.SERVER,
                attributes={"http.route": route}, record_exception=False, set_status_on_exception=False) as span:
            async def observed_send(message):
                nonlocal status, completed
                if message["type"] == "http.response.start":
                    status = message["status"]
                await send(message)
                if message["type"] == "http.response.body" and not message.get("more_body", False):
                    completed = True
            try:
                await self.app(scope, receive, observed_send)
                outcome = ("succeeded" if status < 400 else "rejected" if status < 500 else "provider_error") if completed else "cancelled"
            except (asyncio.CancelledError, OSError):
                outcome = "cancelled"
                raise
            finally:
                INFLIGHT.dec()
                REQUESTS.labels(route, str(status), outcome).inc()
                DURATION.labels(route).observe(time.monotonic() - started)
                span.set_attribute("http.response.status_code", status)
                span.set_attribute("xscope.outcome", outcome)
                if outcome == "provider_error":
                    span.set_status(trace.StatusCode.ERROR)
                LOGGER.info("inference_finished trace_id=%032x outcome=%s duration_seconds=%.4f", span.get_span_context().trace_id, outcome, time.monotonic() - started)
