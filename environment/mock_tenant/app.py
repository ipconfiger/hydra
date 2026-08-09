#!/usr/bin/env python3
"""Mock Tenant Auth Service — always allows all api-keys.
Serves a landing page (GET /), auth endpoint (POST / or /auth), and health (GET /health).
Listens on 0.0.0.0:9091. Stdlib only.
"""
import json, sys
from http.server import HTTPServer, BaseHTTPRequestHandler

LANDING_HTML = """<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>Mock Tenant Auth</title>
<style>
  body{font-family:system-ui,sans-serif;max-width:640px;margin:60px auto;background:#0d1117;color:#e6edf3;padding:0 20px}
  h1{color:#3fb950} code{background:#161b22;padding:2px 6px;border-radius:4px;color:#79c0ff}
  .card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px 20px;margin:16px 0}
</style></head><body>
<h1>🟢 Mock Tenant Auth Service</h1>
<p>This service <strong>always returns <code>{"allowed": true}</code></strong> for any api-key.
It simulates a tenant's authentication endpoint for Hydra e2e testing.</p>
<div class="card"><h3>Endpoints</h3>
<p><code>POST /</code> or <code>POST /auth</code> &rarr; <code>{"allowed": true, "expires_in": 300}</code></p>
<p><code>GET /health</code> &rarr; <code>{"status": "ok"}</code></p></div>
</body></html>"""


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self._json(200, {"status": "ok"})
        else:
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(LANDING_HTML.encode())

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        self._json(200, {"allowed": True, "reason": "mock-tenant-always-allow", "expires_in": 300})

    def _json(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        print(f"[mock-tenant] {args[0] if args else ''}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    print("[mock-tenant] listening on 0.0.0.0:9091", flush=True)
    HTTPServer(("0.0.0.0", 9091), Handler).serve_forever()
