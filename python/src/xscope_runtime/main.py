from __future__ import annotations

import asyncio
import json
import logging
import os
import time
import uuid
from typing import Literal

import uvicorn
from fastapi import FastAPI
from fastapi.responses import StreamingResponse
from fastapi import Request
from pydantic import BaseModel, ConfigDict, Field
from xscope_runtime.telemetry import TelemetryMiddleware, configure


class Message(BaseModel):
    model_config = ConfigDict(extra="forbid")

    role: Literal["system", "user", "assistant"]
    content: str


class StreamOptions(BaseModel):
    include_usage: bool = False


class ChatCompletionRequest(BaseModel):
    model_config = ConfigDict(extra="allow")

    model: str = Field(min_length=1)
    messages: list[Message] = Field(min_length=1)
    stream: bool = False
    stream_options: StreamOptions | None = None


class Usage(BaseModel):
    prompt_tokens: int = Field(ge=0)
    completion_tokens: int = Field(ge=0)
    total_tokens: int = Field(ge=0)


class Choice(BaseModel):
    index: int
    message: Message
    finish_reason: Literal["stop"]


class ChatCompletion(BaseModel):
    id: str
    object: Literal["chat.completion"] = "chat.completion"
    created: int
    model: str
    choices: list[Choice]
    usage: Usage


app = FastAPI(title="XScope Runtime", version="0.1.0")
app.add_middleware(TelemetryMiddleware, tracer=configure())
logger = logging.getLogger("uvicorn.error")


def token_count(text: str) -> int:
    """Deterministic development tokenizer boundary."""
    return len(text.split())


@app.get("/healthz")
def health() -> dict[str, str]:
    return {"status": "ok", "component": "runtime"}


@app.get("/readyz")
def ready() -> dict[str, str]:
    return {"status": "ready"}


@app.post("/v1/chat/completions", response_model=ChatCompletion)
async def chat_completion(request: ChatCompletionRequest, transport: Request):
    logger.info("inference_started request_id=%s", transport.headers.get("x-request-id", "unknown"))
    last_user_message = next(
        (message.content for message in reversed(request.messages) if message.role == "user"), ""
    )
    answer = f"development echo: {last_user_message}"
    prompt_tokens = sum(token_count(message.content) for message in request.messages)
    completion_tokens = token_count(answer)
    completion = ChatCompletion(
        id=f"chatcmpl-{uuid.uuid4().hex}",
        created=int(time.time()),
        model=request.model,
        choices=[
            Choice(
                index=0,
                message=Message(role="assistant", content=answer),
                finish_reason="stop",
            )
        ],
        usage=Usage(
            prompt_tokens=prompt_tokens,
            completion_tokens=completion_tokens,
            total_tokens=prompt_tokens + completion_tokens,
        ),
    )
    if not request.stream:
        return completion

    async def chunks():
        # Starlette cancels this generator on disconnect. A real runtime adapter
        # must cancel its engine request in the same finally block.
        request_id = transport.headers.get("x-request-id", "unknown")
        completed = False
        emitted = 0
        delay = max(0.0, float(os.getenv("XSCOPE_RUNTIME_STREAM_DELAY_SECONDS", "0.02")))

        def event(choices, usage=None):
            return "data: " + json.dumps({
                "id": completion.id,
                "object": "chat.completion.chunk",
                "created": completion.created,
                "model": completion.model,
                "choices": choices,
                "usage": usage,
            }, ensure_ascii=False) + "\n\n"

        try:
            yield event([{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}])
            for index, word in enumerate(answer.split(" ")):
                await asyncio.sleep(delay)
                emitted += 1
                yield event([{
                    "index": 0,
                    "delta": {"content": (" " if index else "") + word},
                    "finish_reason": None,
                }])
            yield event([{"index": 0, "delta": {}, "finish_reason": "stop"}])
            if request.stream_options and request.stream_options.include_usage:
                yield event([], completion.usage.model_dump())
            yield "data: [DONE]\n\n"
            completed = True
        finally:
            logger.info(
                "stream_finished request_id=%s outcome=%s chunks=%d",
                request_id, "completed" if completed else "cancelled", emitted,
            )

    return StreamingResponse(
        chunks(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


def run() -> None:
    uvicorn.run(
        "xscope_runtime.main:app",
        host=os.getenv("XSCOPE_RUNTIME_HOST", "0.0.0.0"),
        port=int(os.getenv("XSCOPE_RUNTIME_PORT", "8090")),
    )


if __name__ == "__main__":
    run()
