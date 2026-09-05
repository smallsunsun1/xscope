# Streaming inference (stage 1)

`POST /v1/chat/completions` accepts `stream: true`. Pingora forwards SSE chunks immediately; it does not concatenate the response. The development FastAPI runtime emits role/content/finish chunks, an optional usage chunk and `[DONE]`. Gateway requests `stream_options.include_usage=true` for accounting. Other request fields are preserved.

The [sse-core decoder](https://docs.rs/sse-core/0.2.3/sse_core/struct.SseDecoder.html) handles incremental UTF-8 and SSE framing. Each accumulated data/name/id field is limited to 64 KiB; total stream length is not capped. The gateway retains only the final usage counters. Identical repeated usage is not added twice; conflicting usage, provider error events, malformed JSON and oversized events terminate forwarding.

Inference request bodies are bounded at 1 MiB and withheld until model permission and quota admission succeed. Upstream request headers can be sent before admission, but no prompt body is released. Upstream framing is chunked because the validated body may gain usage options. Once connected, inference POSTs are not automatically replayed on another backend.

## Cancellation and accounting

Pingora's downstream-close detection drops the upstream HTTP exchange. Starlette then cancels the runtime generator. A production vLLM/other engine adapter must propagate this cancellation into its engine abort API; the echo runtime does not prove GPU engine cancellation.

One WAL event is emitted in the final request logging phase, after downstream success or failure is known:

| Outcome | Event status | Accounting behavior |
| --- | --- | --- |
| Complete response, `[DONE]` for SSE, valid final usage | `succeeded` | Existing metering/ledger charges verified counters |
| Complete protocol response, no final usage | `usage_pending` | Not a zero-token success; financial reconciliation remains pending |
| Downstream disconnected | `cancelled` | Counters preserved if known; interruption settlement belongs to stage 5 |
| Truncated stream/missing `[DONE]`, invalid event or upstream failure | `provider_error` | Not treated as successful usage |

If actual token usage is unknown, the Redis TPM reservation is retained until its quota window expires. It is **not** released as zero usage. Atomic monetary reservation and interruption reconciliation are explicitly stage 5, not implemented by this SSE change.

## Verification

```bash
bazel test //rust/crates/gateway:gateway_test //python:runtime_test //tools:streaming_e2e_test

curl -N http://localhost:30082/v1/chat/completions \
  # Authorization credentials are supplied from a runtime secret.
  -H 'Content-Type: application/json' \
  -d '{"model":"xscope-demo","messages":[{"role":"user","content":"hello streaming model"}],"stream":true}'
```

The integration test starts Bazel-built gateway/runtime binaries on isolated TCP ports. It checks incremental delivery, a single usage event, cancellation after the first content delta, model rejection without starting inference, and non-streaming compatibility. Parser tests cover every two-part UTF-8/CRLF split, long streams, oversized and conflicting usage events.
