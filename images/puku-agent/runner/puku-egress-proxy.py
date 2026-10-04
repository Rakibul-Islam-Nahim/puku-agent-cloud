#!/usr/bin/env python3
"""In-guest egress proxy: turns a policy denial into a legible answer.

The microVM's network policy (msb) is the real enforcement and is not
bypassable from in here. But it denies at the DNS/packet layer, so a blocked
host surfaces to the agent as "Could not resolve host" -- indistinguishable
from a typo or a broken network, which invites the agent to "fix" it.

This proxy sits in front of that and answers 403 on CONNECT instead, the way
a corporate egress gateway would, so the failure reads as a policy decision.
Allowed hosts are tunneled straight through (no TLS interception, so nothing
needs to trust a MITM CA). GET /__agentproxy/status returns the live policy
and a ring buffer of recent denials for the agent to introspect.

Started by puku-runner; the allowlist comes from /session/manifest.json.
"""

import json
import os
import select
import socket
import sys
import threading
from collections import deque
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ALLOW = [d for d in os.environ.get("PUKU_EGRESS_ALLOW", "").split(",") if d]
PORT = int(os.environ.get("PUKU_EGRESS_PROXY_PORT", "3128"))
RECENT_MAX = 32
_recent = deque(maxlen=RECENT_MAX)
_lock = threading.Lock()


def host_allowed(host):
    """Suffix match with a label boundary.

    `puku.sh` matches `puku.sh` and `api-cli.puku.sh`, but must not match
    `puku.sh.evil.test` -- the same semantics msb's DomainSuffix rules use,
    so this proxy never reports allowed for something the VM will deny.
    """
    host = host.lower().rstrip(".")
    for d in ALLOW:
        d = d.lower().strip().rstrip(".")
        if host == d or host.endswith("." + d):
            return True
    return False


def record_denial(host, port):
    with _lock:
        _recent.append({
            "ts": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "kind": "connect_rejected",
            "detail": f"gateway answered 403 for {host}:{port} (not in egress allowlist)",
        })


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "puku-egress-proxy"

    def log_message(self, fmt, *args):  # keep the guest's stderr quiet
        pass

    def _deny(self, host, port):
        record_denial(host, port)
        body = json.dumps({
            "error": "blocked_by_egress_policy",
            "host": host,
            "port": port,
            "message": (
                f"{host}:{port} is not in this environment's egress allowlist. "
                "This is a network policy decision, not a connectivity failure; "
                "it cannot be worked around from inside the sandbox. Ask the "
                "operator to add the domain to PUKU_EGRESS_ALLOW."
            ),
            "allowed_domains": ALLOW,
        }, indent=2).encode()
        self.send_response(403, "Forbidden")
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Proxy-Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _status(self):
        with _lock:
            recent = list(_recent)
        body = json.dumps({
            "enabled": True,
            "port": PORT,
            "enforcement": "msb network policy (deny-by-default); this proxy reports only",
            "tlsInterception": False,
            "allowedDomains": ALLOW,
            "noProxy": ALLOW,
            "recentRelayFailures": recent,
        }, indent=2).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_CONNECT(self):
        host, _, port = self.path.rpartition(":")
        port = int(port or 443)
        if not host_allowed(host):
            self._deny(host, port)
            return
        try:
            upstream = socket.create_connection((host, port), timeout=15)
        except OSError as e:
            self.send_error(502, f"upstream connect failed: {e}")
            return
        self.send_response(200, "Connection established")
        self.end_headers()
        self._tunnel(upstream)

    def _tunnel(self, upstream):
        client = self.connection
        sockets = [client, upstream]
        try:
            while True:
                readable, _, err = select.select(sockets, [], sockets, 60)
                if err or not readable:
                    break
                for s in readable:
                    data = s.recv(65536)
                    if not data:
                        return
                    (upstream if s is client else client).sendall(data)
        except OSError:
            pass
        finally:
            upstream.close()

    def do_GET(self):
        # Origin-form request straight at the proxy: introspection.
        if self.path in ("/__agentproxy/status", "/status"):
            self._status()
            return
        self._forward_plain()

    do_POST = do_PUT = do_DELETE = do_HEAD = do_PATCH = do_GET

    def _forward_plain(self):
        # Absolute-form (http://host/path) is the only proxied plain-HTTP
        # shape; anything allowed should be going direct via NO_PROXY.
        if "://" not in self.path:
            self.send_error(400, "not a proxy request")
            return
        rest = self.path.split("://", 1)[1]
        hostport = rest.split("/", 1)[0]
        host, _, port = hostport.partition(":")
        if not host_allowed(host):
            self._deny(host, int(port or 80))
            return
        self.send_error(
            501, "plain HTTP proxying is not implemented; use HTTPS or direct"
        )


def main():
    if not ALLOW:
        # No allowlist means msb applies no policy either; a proxy that
        # denied everything here would be actively wrong.
        print("puku-egress-proxy: no allowlist, not starting", file=sys.stderr)
        return 0
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    server.daemon_threads = True
    print(f"puku-egress-proxy: listening on 127.0.0.1:{PORT} allow={ALLOW}", file=sys.stderr)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
