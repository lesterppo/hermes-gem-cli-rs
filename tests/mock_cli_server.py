#!/usr/bin/env python3
"""Tiny mock Gemini server for CLI-level testing of gem-cli-rs.

Serves /app tokens, StreamGenerate chat frames, and batchexecute RPCs
(LIST_BOTS, GET_USER_STATUS, LIST_CONVERSATIONS, LIST_CONVERSATION_TURNS,
CREATE_BOT, DELETE_BOT). No live session, no real cookies: the "cookies"
used against it are literal test strings passed via env vars.

Usage: python3 tests/mock_cli_server.py [port]
Prints the chosen port on stdout, then serves forever.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import unquote_plus

TOKENS = (
    '{"SNlM0e":"MOCKAT","cfb2h":"MOCKBL","FdrFJe":"MOCKSID",'
    '"TuX5cc":"en","qKIAYe":"MOCKPUSH"}'
)


def stream_body(text, cid="c_mock", rid="r_mock"):
    inner = [None, [cid, rid], None, None,
             [["rc_mock", [text], None, None, None, None, None, None, [2]]]]
    return ")]}'\n" + json.dumps([None, None, json.dumps(inner)])


def batch_body(rpcid, payload):
    return ")]}'\n" + json.dumps([[None, rpcid, json.dumps(payload)]])


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, body):
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/app":
            self._send(TOKENS)
        else:
            self._send("{}")

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n).decode(errors="replace")
        body = unquote_plus(raw)
        if "StreamGenerate" in self.path:
            text = "Mock follow-up reply" if "follow" in body else "Hello from mock Gemini"
            self._send(stream_body(text))
        elif "batchexecute" in self.path:
            if "CNgdBe" in body:
                self._send(batch_body("CNgdBe", [None, None, [["gem1", ["Test Gem", "A test gem"]]]]))
            elif "otAQ7b" in body:
                models = [None] * 16
                models[15] = [["gemini-3-flash"], ["gemini-3-pro"]]
                self._send(batch_body("otAQ7b", models))
            elif "MaZiqc" in body:
                self._send(batch_body("MaZiqc", [None, "tok", [["c_chat1", "Chat one"]]]))
            elif "hNvQHb" in body:
                # turn[2][0][0] = user text, turn[3][0][0][1][0] = model text
                turn = [None, None, [["Hello"]], [[["?", ["Model reply"]]]]]
                self._send(batch_body("hNvQHb", [[turn]]))
            elif "oMH3Zd" in body:
                self._send(batch_body("oMH3Zd", ["newgem123"]))
            elif "UXcSJb" in body:
                self._send(batch_body("UXcSJb", ["ok"]))
            else:
                self._send(batch_body("UNKNOWN", []))
        else:
            self._send("{}")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    srv = HTTPServer(("127.0.0.1", port), Handler)
    print(srv.server_address[1], flush=True)
    srv.serve_forever()
