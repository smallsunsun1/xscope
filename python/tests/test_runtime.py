import unittest

from fastapi.testclient import TestClient

from xscope_runtime.main import app


class RuntimeTest(unittest.TestCase):
    client = TestClient(app)

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

    def test_streaming_is_rejected_by_schema(self) -> None:
        response = self.client.post(
            "/v1/chat/completions",
            json={
                "model": "xscope-demo",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": True,
            },
        )
        self.assertEqual(response.status_code, 422)


if __name__ == "__main__":
    unittest.main()
