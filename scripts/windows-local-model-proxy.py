#!/usr/bin/env python3
"""Forward WSL loopback requests to the Windows-only model using Windows curl."""
import subprocess
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CURL = "/mnt/c/Windows/System32/curl.exe"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def forward(self):
        if not self.path.startswith("/v1/") or "\r" in self.path or "\n" in self.path:
            self.send_error(404)
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        args = [CURL, "--silent", "--show-error", "--no-buffer", "--include",
                "--noproxy", "*", "--connect-timeout", "5", "--max-time", "600",
                "--request", self.command, "--header", "Authorization: Bearer local",
                "--header", "Content-Type: application/json"]
        if self.command == "POST":
            args += ["--data-binary", "@-"]
        args += ["http://127.0.0.1:8080" + self.path]
        process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL)
        try:
            process.stdin.write(body)
            process.stdin.close()
            # Ignore interim 100 Continue responses.
            while True:
                status = process.stdout.readline().decode("latin1").strip()
                if not status.startswith("HTTP/"):
                    self.send_error(502, "Windows model service unavailable")
                    return
                headers = []
                while True:
                    line = process.stdout.readline()
                    if line in (b"\r\n", b"\n", b""):
                        break
                    headers.append(line.decode("latin1").strip())
                code = int(status.split()[1])
                if code != 100:
                    break
            # curl decodes upstream transfer framing; close delimits our response.
            self.send_response(code)
            for header in headers:
                name, value = header.split(":", 1)
                if name.lower() in ("content-type", "cache-control"):
                    self.send_header(name, value.strip())
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            while chunk := process.stdout.read1(8192):
                self.wfile.write(chunk)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
