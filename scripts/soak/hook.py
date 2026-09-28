#!/usr/bin/env python3
"""A farm webhook for scripts/soak.sh (dev tool): answers 204 to every POST
and appends one JSON line per delivery (time, path, body) to --out."""

import argparse
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    out = open(a.out, "a")

    class H(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("content-length") or 0))
            try:
                parsed = json.loads(body)
            except ValueError:
                parsed = body.decode("utf-8", "replace")
            out.write(json.dumps({"t": round(time.time(), 1), "path": self.path, "body": parsed}) + "\n")
            out.flush()
            self.send_response(204)
            self.end_headers()

        def log_message(self, *args):
            pass

    ThreadingHTTPServer(("127.0.0.1", a.port), H).serve_forever()


if __name__ == "__main__":
    main()
