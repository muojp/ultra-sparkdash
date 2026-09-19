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


@pytest.fixture(scope="session")
def thinking_probe():
    return _load("llm-thinking-probe")


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
            message = {"content": "ok"}
            reasoning_tokens = 0
            if self.server.reasons and not self._switched_off(body):
                # A model that thinks unless the right variable reaches its template. Where it
                # says so differs: most fill reasoning_content, Flash-Next reports the tokens in
                # usage and leaves the field empty (`reasoning_in_usage_only`).
                if self.server.reasoning_in_usage_only:
                    reasoning_tokens = 198
                else:
                    message["reasoning_content"] = "let me think about it"
            self._json({"choices": [{"message": message, "finish_reason": "stop"}],
                        "usage": {"completion_tokens": n, "prompt_tokens": prompt_tokens,
                                  "completion_tokens_details": {"reasoning_tokens": reasoning_tokens},
                                  "prompt_tokens_details": {"cached_tokens": 0}}})

    def _switched_off(self, body):
        """True when the request carries the one spelling this stub's template defines.

        The point of the stub: every other spelling is *ignored*, exactly as a chat template
        ignores a variable it does not define, so a probe that only tries one name sees a model
        that "always thinks" instead of a name it never sent.
        """
        key = self.server.honours
        if key is None:
            return False
        if key == "reasoning_effort":
            return body.get("reasoning_effort") == "none"
        return (body.get("chat_template_kwargs") or {}).get(key) is False

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


def make_server(model_id="stub-model", reject_thinking=False, reasoning_key=False,
                reasons=False, honours=None, reasoning_in_usage_only=False):
    """`reasons` makes the stub emit a reasoning trace; `honours` is the one spelling that stops it.

    `reasoning_in_usage_only` reports that trace as usage tokens with an empty reasoning_content,
    which is what Qwen3.8-Flash-Next does on a non-streamed response.
    """
    srv = HTTPServer(("127.0.0.1", 0), StubHandler)
    srv.model_id = model_id
    srv.reject_thinking = reject_thinking
    srv.reasoning_key = reasoning_key
    srv.reasons = reasons
    srv.honours = honours
    srv.reasoning_in_usage_only = reasoning_in_usage_only
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
