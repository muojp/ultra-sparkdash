"""Load the bin/ scripts as modules.

They are executables without a .py suffix — that is the interface they are used through — so the
tests import them by path rather than renaming them for the test suite's convenience.
"""
import importlib.machinery
import importlib.util
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

BIN = Path(__file__).resolve().parent.parent / "bin"


def _load(name):
    # No .py suffix on these, so the loader has to be named explicitly.
    loader = importlib.machinery.SourceFileLoader(name.replace("-", "_"), str(BIN / name))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


@pytest.fixture(scope="session")
def quickbench():
    return _load("llm-quickbench")


@pytest.fixture(scope="session")
def longctx():
    return _load("llm-longctx-probe")


@pytest.fixture(scope="session")
def dgx_model():
    return _load("dgx-model")


class StubHandler(BaseHTTPRequestHandler):
    """Minimal OpenAI-compatible server: /v1/models and a streaming /v1/chat/completions."""

    def log_message(self, *a):  # silence
        pass

    def do_GET(self):
        if self.path.endswith("/models"):
            self._json({"data": [{"id": self.server.model_id, "max_model_len": 262144}]})
        else:
            self.send_error(404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(body)
        if "chat_template_kwargs" in body and self.server.reject_thinking:
            self.send_error(400, "template has no thinking variable")
            return
        n = body.get("max_tokens", 8)
        prompt_tokens = max(1, len(body["messages"][0]["content"]) // 4)
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for i in range(n):
                key = "reasoning_content" if self.server.reasoning_key else "content"
                self._sse({"choices": [{"delta": {key: "x"}}]})
            self._sse({"choices": [{"delta": {}, "finish_reason": "length"}],
                       "usage": {"completion_tokens": n, "prompt_tokens": prompt_tokens,
                                 "completion_tokens_details": {"reasoning_tokens": 0},
                                 "prompt_tokens_details": {"cached_tokens": 0}}})
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            self._json({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                        "usage": {"completion_tokens": n, "prompt_tokens": prompt_tokens,
                                  "completion_tokens_details": {"reasoning_tokens": 0},
                                  "prompt_tokens_details": {"cached_tokens": 0}}})

    def _json(self, obj):
        raw = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _sse(self, obj):
        self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
        self.wfile.flush()


def make_server(model_id="stub-model", reject_thinking=False, reasoning_key=False):
    srv = HTTPServer(("127.0.0.1", 0), StubHandler)
    srv.model_id = model_id
    srv.reject_thinking = reject_thinking
    srv.reasoning_key = reasoning_key
    srv.requests = []
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    return srv


@pytest.fixture
def stub():
    srv = make_server()
    yield srv
    srv.shutdown()


@pytest.fixture
def stub_pair():
    a, b = make_server(), make_server()
    yield a, b
    a.shutdown()
    b.shutdown()


def url(srv):
    return "http://127.0.0.1:%d/v1" % srv.server_port


@pytest.fixture(scope="session")
def report():
    return _load("llm-bench-report")
