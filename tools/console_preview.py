"""Read-only preview of Bazel-built UI against the existing local console.

Log in at localhost:30081 first; the browser's existing localhost cookie is
forwarded only to that fixed local destination. No internal token or DB access.
"""
import http.client
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import sys

from python.runfiles import runfiles


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=DIRECTORY, **kwargs)

    def do_GET(self):
        if not self.path.startswith("/api/"):
            return super().do_GET()
        connection = http.client.HTTPConnection("127.0.0.1", 30081, timeout=15)
        try:
            connection.request("GET", self.path, headers={"Host": "localhost:30081", "Accept": "application/json", "Cookie": self.headers.get("Cookie", "")})
            response = connection.getresponse()
            raw = response.read()
            if 300 <= response.status < 400:
                self.send_error(401, "Log in at http://localhost:30081 first")
                return
            self.send_response(response.status)
            self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
            self.send_header("Content-Length", str(len(raw)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(raw)
        finally:
            connection.close()


if __name__ == "__main__":
    DIRECTORY = runfiles.Create().Rlocation(sys.argv[1])
    print("Read-only UI preview: http://localhost:4173 (mutations are disabled)", flush=True)
    ThreadingHTTPServer(("127.0.0.1", 4173), Handler).serve_forever()
