from __future__ import annotations

import os
import time
import uuid
from typing import Literal

import uvicorn
from fastapi import FastAPI
from pydantic import BaseModel, ConfigDict, Field


class Message(BaseModel):
    model_config = ConfigDict(extra="forbid")

    role: Literal["system", "user", "assistant"]
    content: str


class ChatCompletionRequest(BaseModel):
    model_config = ConfigDict(extra="allow")

    model: str = Field(min_length=1)
    messages: list[Message] = Field(min_length=1)
    stream: Literal[False] = False


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
def chat_completion(request: ChatCompletionRequest) -> ChatCompletion:
    last_user_message = next(
        (message.content for message in reversed(request.messages) if message.role == "user"), ""
    )
    answer = f"development echo: {last_user_message}"
    prompt_tokens = sum(token_count(message.content) for message in request.messages)
    completion_tokens = token_count(answer)
    return ChatCompletion(
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


def run() -> None:
    uvicorn.run(
        "xscope_runtime.main:app",
        host=os.getenv("XSCOPE_RUNTIME_HOST", "0.0.0.0"),
        port=int(os.getenv("XSCOPE_RUNTIME_PORT", "8090")),
    )


if __name__ == "__main__":
    run()
