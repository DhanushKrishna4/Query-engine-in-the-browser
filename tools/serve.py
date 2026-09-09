#!/usr/bin/env python3
"""Serve web/ with caching turned off.

    python3 tools/serve.py [port]

`python -m http.server` sends Last-Modified and lets the browser cache, which
during development means editing Rust, rebuilding the wasm, reloading, and
still running the old module. That is a genuinely confusing failure -- it looks
like the fix did not work -- so this sends `Cache-Control: no-store` instead.

It also serves .wasm with the right MIME type, which `WebAssembly.instantiateStreaming`
insists on.
"""
import functools
import http.server
import os
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "web")


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
    }

    def end_headers(self):
        self.send_header("Cache-Control", "no-store, must-revalidate")
        super().end_headers()

    def log_message(self, fmt, *args):  # quieter than the default one-line-per-request
        if not args or "200" not in str(args):
            super().log_message(fmt, *args)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8137
    handler = functools.partial(Handler, directory=os.path.normpath(ROOT))
    with http.server.ThreadingHTTPServer(("127.0.0.1", port), handler) as httpd:
        print(f"serving {os.path.normpath(ROOT)} on http://localhost:{port}")
        httpd.serve_forever()


if __name__ == "__main__":
    main()
