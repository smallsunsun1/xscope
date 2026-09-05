"""Two Bazel gateways and a disposable local Redis, with lost-reply injection.

No changes to Kubernetes or its Redis. Docker fixture is CPU/memory bounded.
"""
import http.client
import json
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from python.runfiles import runfiles


def eventually(check, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError("quota recovery timed out")


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def resp(reader):
    """Small test-only RESP framing reader; Redis executes the real Lua scripts."""
    line = reader.readline()
    if not line:
        raise EOFError()
    kind, body = line[:1], line[1:-2]
    if kind in (b"$", b"!", b"="):
        size = int(body)
        if size == -1:
            return None, line
        tail = reader.read(size + 2)
        if len(tail) != size + 2:
            raise EOFError()
        return tail[:-2], line + tail
    if kind in (b"*", b"%", b"~", b">"):
        count = int(body) * (2 if kind == b"%" else 1)
        values, raw = [], bytearray(line)
        for _ in range(max(0, count)):
            value, chunk = resp(reader)
            values.append(value)
            raw.extend(chunk)
        return values, bytes(raw)
    return body, line


def encode(command):
    return b"*%d\r\n" % len(command) + b"".join(b"$%d\r\n" % len(v) + v + b"\r\n" for v in command)


def main():
    resolver = runfiles.Create()
    gateway, runtime = [resolver.Rlocation(arg) for arg in sys.argv[1:3]]
    name = "xscope-quota-smoke-" + uuid.uuid4().hex[:10]
    image = "redis:8.4.6-alpine"
    processes, logs = [], []
    container = None
    proxy = None
    with tempfile.TemporaryDirectory(prefix="xscope-quota-") as directory:
        root = Path(directory)
        try:
            # Use a locally available development image; no replacement of the cluster Redis.
            subprocess.run(["docker", "image", "inspect", image], check=True, stdout=subprocess.DEVNULL)
            container = subprocess.check_output(["docker", "run", "--rm", "-d", "--name", name, "--cpus", "0.25", "--memory", "96m",
                "-p", "127.0.0.1::6379", image, "redis-server", "--save", "", "--appendonly", "no"], text=True).strip()
            mapping = subprocess.check_output(["docker", "port", container, "6379/tcp"], text=True).strip()
            redis_port = int(mapping.rsplit(":", 1)[1])
            dropped = {"reserve": False, "settle": False}
            settlements = []

            class FaultProxy(socketserver.StreamRequestHandler):
                def handle(self):
                    try:
                        with socket.create_connection(("127.0.0.1", redis_port), timeout=3) as upstream:
                            upstream.settimeout(5)
                            with upstream.makefile("rb") as reader:
                                while True:
                                    command, wire = resp(self.rfile)
                                    upstream.sendall(wire)
                                    _, response = resp(reader)
                                    if isinstance(command, list) and command[0].upper() == b"EVALSHA" and not response.startswith(b"-"):
                                        operation = "reserve" if response.startswith(b"*") else "settle"
                                        if operation == "settle":
                                            settlements.append(command)
                                        if not dropped[operation]:
                                            dropped[operation] = True
                                            return  # Applied by Redis, response intentionally lost.
                                    self.wfile.write(response)
                                    self.wfile.flush()
                    except (EOFError, OSError):
                        return

            class Server(socketserver.ThreadingTCPServer):
                daemon_threads = True

            proxy = Server(("127.0.0.1", 0), FaultProxy)
            threading.Thread(target=proxy.serve_forever, daemon=True).start()

            def spawn(binary, env, label):
                log = (root / (label + ".log")).open("wb")
                logs.append(log)
                processes.append(subprocess.Popen([binary], env=dict(os.environ, **env), stdout=log, stderr=log))

            def ready(listen):
                try:
                    c = http.client.HTTPConnection("127.0.0.1", listen, timeout=1)
                    c.request("GET", "/readyz")
                    r = c.getresponse()
                    result = r.status == 200
                    r.read()
                    c.close()
                    return result
                except OSError:
                    return False

            runtime_port = port()
            spawn(runtime, {"XSCOPE_RUNTIME_HOST": "127.0.0.1", "XSCOPE_RUNTIME_PORT": str(runtime_port), "XSCOPE_RUNTIME_STREAM_DELAY_SECONDS": "0"}, "runtime")
            eventually(lambda: ready(runtime_port))
            gateway_ports = []
            for index in range(2):
                listen = port()
                spawn(gateway, {"XSCOPE_GATEWAY_ADDRESS": f"127.0.0.1:{listen}", "XSCOPE_METRICS_ADDRESS": f"127.0.0.1:{port()}",
                    "XSCOPE_USAGE_WAL": str(root / f"gateway-{index}.jsonl"), "XSCOPE_CONTROL_INTERNAL_URL": "", "XSCOPE_INTERNAL_TOKEN": "",
                    "XSCOPE_REDIS_URL": f"redis://127.0.0.1:{proxy.server_address[1]}/", "XSCOPE_ADDITIONAL_SERVING_JSON": "[]",
                    "XSCOPE_SERVING_ENTRY_JSON": json.dumps({"id": "smoke-pool", "model": "xscope-demo", "revision": "v1", "address": f"127.0.0.1:{runtime_port}"}),
                    "XSCOPE_API_KEYS_JSON": json.dumps([{"id": "smoke-key", "project_id": "smoke", "tenant_id": "smoke", "secret": "smoke-secret"}])}, f"gateway-{index}")
                gateway_ports.append(listen)
                eventually(lambda: ready(listen))

            def infer(listen):
                c = http.client.HTTPConnection("127.0.0.1", listen, timeout=5)
                c.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "stream": False, "max_tokens": 8,
                    "messages": [{"role": "user", "content": "hello"}]}), {"Authorization": "Bearer smoke-secret", "Content-Type": "application/json"})
                response = c.getresponse()
                raw = response.read()
                assert response.status == 200, (response.status, raw)
                c.close()
                usage = json.loads(raw)["usage"]
                return usage["prompt_tokens"] + usage["completion_tokens"]

            def command(*args):
                with socket.create_connection(("127.0.0.1", redis_port), timeout=3) as connection:
                    connection.sendall(encode([a.encode() if isinstance(a, str) else a for a in args]))
                    with connection.makefile("rb") as reader:
                        return resp(reader)

            def totals():
                keys, _ = command("KEYS", "xscope:quota:*")  # Disposable Redis only.
                return tuple(sum(int(command("HGET", key, field)[0] or 0) for key in keys) for field in ("requests", "tokens"))

            actual = infer(gateway_ports[0])
            eventually(lambda: all(dropped.values()) and totals() == (1, actual))
            print("PASS lost reserve/settle replies reconnect and retry without double counting", flush=True)
            # Repeat the captured settlement, then deliberately alter its payload.
            settled = settlements[-1]
            assert not command(*settled)[1].startswith(b"-")
            conflicting = settled.copy()
            conflicting[-2] = str(int(conflicting[-2]) + 1).encode()
            assert command(*conflicting)[1].startswith(b"-"), "conflicting settlement was accepted"
            assert totals() == (1, actual)
            print("PASS duplicate settlement is a no-op; payload conflict is rejected", flush=True)

            actual += infer(gateway_ports[1])
            eventually(lambda: totals() == (2, actual))
            # Kill Redis clients, not Redis or its state, to reproduce a stale connection.
            command("CLIENT", "KILL", "TYPE", "normal", "SKIPME", "yes")
            actual += infer(gateway_ports[0])
            eventually(lambda: totals() == (3, actual))
            print("PASS two gateways share exact counters; first request after stale connection returns 200", flush=True)
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
            for log in logs:
                log.close()
            if proxy:
                proxy.shutdown()
                proxy.server_close()
            if container:
                subprocess.run(["docker", "rm", "-f", container], check=True, stdout=subprocess.DEVNULL)
            print("Removed only disposable quota-test container and local processes; cluster Redis/data unchanged.", flush=True)


if __name__ == "__main__":
    main()
