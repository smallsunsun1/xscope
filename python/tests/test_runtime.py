import json
import unittest

from fastapi.testclient import TestClient

from xscope_runtime.main import app


class RuntimeTest(unittest.TestCase):
    client = TestClient(app)

    def test_telemetry_initialization_is_idempotent(self) -> None:
        from xscope_runtime.telemetry import configure
        self.assertIs(configure(), configure())

    def test_health(self) -> None:
        response = self.client.get("/healthz")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["component"], "runtime")

    def test_chat_completion_has_openai_shape_and_usage(self) -> None:
        response = self.client.post(
            "/v1/chat/completions",
            json={
                "model": "xscope-demo",
                "messages": [
                    {"role": "system", "content": "Be concise"},
                    {"role": "user", "content": "hello model"},
                ],
            },
        )
        self.assertEqual(response.status_code, 200)
        body = response.json()
        self.assertEqual(body["object"], "chat.completion")
        self.assertEqual(
            body["choices"][0]["message"]["content"],
            "development echo: hello model",
        )
        self.assertEqual(
            body["usage"],
            {
                "prompt_tokens": 4,
                "completion_tokens": 4,
                "total_tokens": 8,
            },
        )

    def test_streaming_has_incremental_deltas_and_final_usage(self) -> None:
        response = self.client.post(
            "/v1/chat/completions",
            json={
                "model": "xscope-demo",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": True,
                "stream_options": {"include_usage": True},
            },
        )
        self.assertEqual(response.status_code, 200)
        self.assertIn("text/event-stream", response.headers["content-type"])
        frames = [line[6:] for line in response.text.splitlines() if line.startswith("data: ")]
        self.assertEqual(frames.pop(), "[DONE]")
        chunks = [json.loads(frame) for frame in frames]
        answer = "".join(choice["delta"].get("content", "") for chunk in chunks for choice in chunk["choices"])
        self.assertEqual(answer, "development echo: hello")
        usage = [chunk["usage"] for chunk in chunks if chunk["usage"] is not None]
        self.assertEqual(usage, [{"prompt_tokens": 1, "completion_tokens": 3, "total_tokens": 4}])
        self.assertTrue(all(chunk["object"] == "chat.completion.chunk" for chunk in chunks))

    def test_stream_usage_is_opt_in(self) -> None:
        response = self.client.post("/v1/chat/completions", json={
            "model": "xscope-demo", "messages": [{"role": "user", "content": "hello"}], "stream": True,
        })
        chunks = [json.loads(line[6:]) for line in response.text.splitlines()
                  if line.startswith("data: {")]
        self.assertTrue(all(chunk["usage"] is None for chunk in chunks))


if __name__ == "__main__":
    unittest.main()
