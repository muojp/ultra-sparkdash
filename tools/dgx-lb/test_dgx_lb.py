"""Fixture tests for dgx-lb: fake vLLM upstreams on ephemeral ports, the LB in-process.

The fake upstream's behaviour is chosen per request by the X-Fake-Mode header (the LB forwards
client headers), or per server by `FakeUpstream.force`.
"""
import http.client
import json
import os
import re
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

sys.path.insert(0, os.path.dirname(__file__))
import dgx_lb  # noqa: E402

SERVED = "qwen3.8-flash-next-single"


class FakeUpstream:
    def __init__(self, served=SERVED, port=0):
        self.served = served
        self.force = None
        self.inflight = 0
        self.max_inflight = 0
        self.lock = threading.Lock()
        self.seen = []            # (path, headers, body)
        self.gen_total = 0
        self.port = port
        self.start()

    def start(self):
        up = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _json(self, code, obj):
                b = json.dumps(obj).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(b)))
                self.end_headers()
                self.wfile.write(b)

            def do_GET(self):
                if self.path == "/v1/models":
                    return self._json(200, {"object": "list", "data": [{"id": up.served}]})
                if self.path == "/metrics":
                    t = (f'vllm:num_requests_running{{model_name="{up.served}"}} {up.inflight}\n'
                         f'vllm:num_requests_waiting{{model_name="{up.served}"}} 0\n'
                         f'vllm:num_requests_waiting_by_reason{{model_name="{up.served}",reason="capacity"}} 0\n'
                         f'vllm:generation_tokens_total{{model_name="{up.served}"}} {up.gen_total}\n'
                         f'vllm:prompt_tokens_total{{model_name="{up.served}"}} 0\n').encode()
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(t)))
                    self.end_headers()
                    self.wfile.write(t)
                    return
                self._json(404, {"error": "nope"})

            def do_POST(self):
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n)
                up.seen.append((self.path, dict(self.headers), body))
                mode = up.force or self.headers.get("X-Fake-Mode", "json")
                with up.lock:
                    up.inflight += 1
                    up.max_inflight = max(up.max_inflight, up.inflight)
                try:
                    self._serve(mode, body)
                finally:
                    with up.lock:
                        up.inflight -= 1

            def _serve(self, mode, body):
                opts = dict(p.split("=", 1) if "=" in p else (p, "1") for p in mode.split(","))
                if "hold" in opts:
                    time.sleep(float(opts["hold"]))
                if "slow_first" in opts:
                    time.sleep(float(opts["slow_first"]))
                if "status" in opts:
                    return self._json(int(opts["status"]), {"error": "forced"})
                if "json" in opts:
                    usage = {"prompt_tokens": 11, "completion_tokens": 7}
                    if "reasoning" in opts:
                        usage["completion_tokens_details"] = {"reasoning_tokens": int(opts["reasoning"])}
                    up.gen_total += 7
                    return self._json(200, {"choices": [{"message": {"content": "hi"}}], "usage": usage,
                                            "echo": json.loads(body or b"{}")})
                # streamed modes
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()

                def chunk(data: bytes):
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
                    self.wfile.flush()
                if "sse_chat" in opts:
                    chunk(b'data: {"choices":[{"delta":{"content":"a"}}]}\n\n')
                    chunk(b'data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3,'
                          b'"completion_tokens_details":{"reasoning_tokens":2}}}\n\n')
                    chunk(b"data: [DONE]\n\n")
                    self.wfile.write(b"0\r\n\r\n")
                    up.gen_total += 3
                    return
                if "sse_resp" in opts:
                    chunk(b'event: response.output_text.delta\ndata: {"type":"response.output_text.delta","delta":"x"}\n\n')
                    chunk(b'event: response.completed\ndata: {"type":"response.completed","response":{"usage":'
                          b'{"input_tokens":20,"output_tokens":9,"output_tokens_details":{"reasoning_tokens":4}}}}\n\n')
                    self.wfile.write(b"0\r\n\r\n")
                    up.gen_total += 9
                    return
                if "midstream" in opts:
                    chunk(b'data: {"choices":[{"delta":{"content":"a"}}]}\n\n')
                    chunk(b'data: {"choices":[{"delta":{"content":"b"}}]}\n\n')
                    self.wfile.flush()
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.close_connection = True
                    return
                if "slow_idle" in opts:
                    chunk(b'data: {"choices":[{"delta":{"content":"a"}}]}\n\n')
                    time.sleep(float(opts["slow_idle"]))
                    chunk(b"data: [DONE]\n\n")
                    self.wfile.write(b"0\r\n\r\n")
                    return
                if "drip" in opts:  # one event every 0.2 s for `drip` seconds
                    end = time.time() + float(opts["drip"])
                    try:
                        while time.time() < end:
                            chunk(b'data: {"choices":[{"delta":{"content":"a"}}]}\n\n')
                            time.sleep(0.2)
                        chunk(b"data: [DONE]\n\n")
                        self.wfile.write(b"0\r\n\r\n")
                    except OSError:
                        pass
                    return

        ThreadingHTTPServer.allow_reuse_address = True
        self.srv = ThreadingHTTPServer(("127.0.0.1", self.port), H)
        self.srv.daemon_threads = True
        self.port = self.srv.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}"
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def stop(self):
        self.srv.shutdown()
        self.srv.server_close()


def make_lb(tmp_path, ups, caps=None, profiles=None, timeouts=None, health=None, allow_short=True, **extra):
    cfg = {
        "served_model": SERVED,
        "listen_host": "127.0.0.1",
        "ledger_dir": str(tmp_path / "ledger"),
        "upstream": [{"name": f"n{i + 1}", "url": u.url, "cap": (caps or [4] * len(ups))[i]} for i, u in enumerate(ups)],
        "profile": profiles or [{"name": "plain", "port": 0}],
        "timeouts": timeouts or {"connect": 1, "first_byte": 5, "idle": 5, "total": 10},
        "health": health or {"interval": 60, "probe_timeout": 1, "eject_after": 2, "readmit_after": 2},
        "retry": {"max_attempts": 3, "backoff": [0.05, 0.05, 0.05]},
        **extra,
    }
    lb = dgx_lb.LB(dgx_lb.load_config(data=cfg), allow_short=allow_short)
    lb.start()
    return lb


def req(lb, body=None, mode="json", path="/v1/chat/completions", headers=None, profile="plain", method="POST"):
    """One request through the LB; returns once its ledger row exists (the row is written after the
    response has gone out, so reading the ledger straight away would race)."""
    before = len(ledger(lb))
    c = http.client.HTTPConnection("127.0.0.1", lb.ports[profile], timeout=30)
    h = {"Content-Type": "application/json", "X-Fake-Mode": mode, **(headers or {})}
    data = json.dumps(body if body is not None else {"model": SERVED, "messages": []}).encode() if method == "POST" else None
    c.request(method, path, body=data, headers=h)
    r = c.getresponse()
    try:
        payload = r.read()
    except http.client.IncompleteRead as e:
        payload = e.partial
    c.close()
    wait_for(lambda: len(ledger(lb)) > before, 10)
    return r, payload


def ledger(lb):
    rows = []
    if not os.path.isdir(lb.ledger.dir):
        return rows
    for f in sorted(os.listdir(lb.ledger.dir)):
        rows += [json.loads(l) for l in open(os.path.join(lb.ledger.dir, f))]
    return rows


def wait_for(pred, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.02)
    return False


def parse_metrics(text):
    out = {}
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        m = re.match(r'^([a-zA-Z_:]+)(\{[^}]*\})?\s+(\S+)$', line)
        labels = dict(re.findall(r'(\w+)="([^"]*)"', m.group(2) or ""))
        out.setdefault(m.group(1), []).append((labels, float(m.group(3))))
    return out


def metrics(lb):
    c = http.client.HTTPConnection("127.0.0.1", lb.ports["plain"], timeout=5)
    c.request("GET", "/metrics")
    r = c.getresponse()
    assert r.status == 200
    return parse_metrics(r.read().decode())


@pytest.fixture
def two(tmp_path):
    a, b = FakeUpstream(), FakeUpstream()
    lb = make_lb(tmp_path, [a, b], caps=[2, 2])
    yield lb, a, b
    lb.stop()
    for u in (a, b):
        try:
            u.stop()
        except Exception:
            pass


# 1 ------------------------------------------------------------------------------------------------
def test_distribution_respects_caps_and_queues(two):
    lb, a, b = two
    results = []
    ths = [threading.Thread(target=lambda: results.append(req(lb, mode="json,hold=0.3")[0].status)) for _ in range(8)]
    [t.start() for t in ths]
    [t.join() for t in ths]
    assert results == [200] * 8
    assert a.max_inflight <= 2 and b.max_inflight <= 2
    assert len(a.seen) >= 3 and len(b.seen) >= 3
    rows = ledger(lb)
    assert sum(1 for r in rows if r["queue_wait_s"] > 0.1) >= 3  # 8 requests, 4 slots: some queued here


# 2 ------------------------------------------------------------------------------------------------
def test_connect_refused_retries_other_node_then_ejects_and_readmits(two):
    lb, a, b = two
    port_a = a.port
    a.stop()
    r, _ = req(lb)
    assert r.status == 200 and r.getheader("X-Dgx-Lb-Node") == "n2"
    row = ledger(lb)[-1]
    assert row["attempts"] == 2 and row["outcome"] == "retried_ok" and row["node"] == "n2"
    assert lb.metrics.get("lb_retries_total", served_model=SERVED, node="n1", profile="plain") == 1
    req(lb)  # least-outstanding tie-break may pick n1 again -> second failure ejects it
    wait_for(lambda: not lb.pool.nodes[0].up or ledger(lb)[-1]["attempts"] == 1, 2)
    lb.pool.probe_all()
    assert lb.pool.nodes[0].up is False
    a2 = FakeUpstream(port=port_a)
    try:
        lb.pool.probe_all()
        assert lb.pool.nodes[0].up is False  # one good probe is not enough
        lb.pool.probe_all()
        assert lb.pool.nodes[0].up is True
    finally:
        a2.stop()


# 3 ------------------------------------------------------------------------------------------------
def test_503_before_headers_is_retried_and_400_is_not(two):
    lb, a, b = two
    a.force = "status=503"
    r, _ = req(lb)
    assert r.status == 200
    assert ledger(lb)[-1]["attempts"] == 2
    a.force = None
    lb.pool.probe_all(); lb.pool.probe_all()
    r, _ = req(lb, mode="status=400")
    assert r.status == 400
    row = ledger(lb)[-1]
    assert row["attempts"] == 1 and row["status"] == 400


# 4 ------------------------------------------------------------------------------------------------
def test_midstream_break_is_not_retried(two):
    lb, a, b = two
    r, payload = req(lb, mode="midstream", body={"model": SERVED, "stream": True})
    assert r.status == 200
    assert b'"a"' in payload
    row = ledger(lb)[-1]
    assert row["outcome"] == "midstream_broken" and row["attempts"] == 1
    assert len(a.seen) + len(b.seen) == 1


# 5 ------------------------------------------------------------------------------------------------
def test_timeouts_each_have_their_outcome(tmp_path):
    a = FakeUpstream()
    lb = make_lb(tmp_path, [a], timeouts={"connect": 1, "first_byte": 0.5, "idle": 0.5, "total": 1.5})
    try:
        r, _ = req(lb, mode="slow_first=1.5")
        assert r.status == 504 and ledger(lb)[-1]["outcome"] == "timeout_first_byte"
        lb.pool.probe_all(); lb.pool.probe_all()
        r, _ = req(lb, mode="slow_idle=1.2", body={"stream": True})
        assert ledger(lb)[-1]["outcome"] == "timeout_idle"
        lb.pool.probe_all(); lb.pool.probe_all()
        r, _ = req(lb, mode="drip=3", body={"stream": True})
        assert ledger(lb)[-1]["outcome"] == "timeout_total"
    finally:
        lb.stop(); a.stop()


def test_startup_refuses_short_timeouts_bp60(tmp_path):
    a = FakeUpstream()
    try:
        with pytest.raises(SystemExit):
            make_lb(tmp_path, [a], timeouts={"connect": 5, "first_byte": 900, "idle": 300, "total": 20}, allow_short=False)
        with pytest.raises(SystemExit):
            make_lb(tmp_path, [a], timeouts={"connect": 5, "first_byte": 20, "idle": 300, "total": 3900}, allow_short=False)
        lb = make_lb(tmp_path, [a], timeouts={"connect": 5, "first_byte": 900, "idle": 300, "total": 3900}, allow_short=False)
        lb.stop()
    finally:
        a.stop()


# 6 ------------------------------------------------------------------------------------------------
def test_wrong_served_model_is_excluded(tmp_path):
    a, b = FakeUpstream(served="some-other-model"), FakeUpstream()
    lb = make_lb(tmp_path, [a, b])
    try:
        assert lb.pool.nodes[0].up is False and lb.pool.nodes[1].up is True
        for _ in range(4):
            assert req(lb)[0].status == 200
        assert len(a.seen) == 0
        b.served = "switched-away"
        lb.pool.probe_all()
        c = http.client.HTTPConnection("127.0.0.1", lb.ports["plain"], timeout=5)
        c.request("GET", "/health"); h = c.getresponse(); h.read()
        assert h.status == 503
        c.request("GET", "/v1/models"); m = c.getresponse(); m.read()
        assert m.status == 503
        assert req(lb)[0].status == 503 and ledger(lb)[-1]["outcome"] == "no_upstream"
    finally:
        lb.stop(); a.stop(); b.stop()


# 7 ------------------------------------------------------------------------------------------------
def test_profile_inject_merges_and_keeps_other_kwargs(tmp_path):
    a = FakeUpstream()
    lb = make_lb(tmp_path, [a], profiles=[{"name": "plain", "port": 0},
                                          {"name": "think", "port": 0,
                                           "inject": {"chat_template_kwargs": {"enable_thinking": True}}}])
    try:
        body = {"model": SERVED, "chat_template_kwargs": {"foo": 1, "enable_thinking": False}}
        r, payload = req(lb, body=body, profile="think")
        echo = json.loads(payload)["echo"]
        assert echo["chat_template_kwargs"] == {"foo": 1, "enable_thinking": True}
        r, payload = req(lb, body=body, profile="plain")
        assert json.loads(payload)["echo"]["chat_template_kwargs"] == {"foo": 1, "enable_thinking": False}
        assert ledger(lb)[-2]["profile"] == "think"
    finally:
        lb.stop(); a.stop()


# 8 ------------------------------------------------------------------------------------------------
def test_usage_parsed_from_json_chat_sse_and_responses_sse(two):
    lb, a, b = two
    req(lb, mode="json,reasoning=5")
    req(lb, mode="sse_chat", body={"stream": True})
    req(lb, mode="sse_resp", path="/v1/responses", body={"stream": True})
    req(lb, mode="json")  # qs-style: no reasoning field -> stays null
    rows = ledger(lb)[-4:]
    assert (rows[0]["prompt_tokens"], rows[0]["completion_tokens"], rows[0]["reasoning_tokens"]) == (11, 7, 5)
    assert (rows[1]["prompt_tokens"], rows[1]["completion_tokens"], rows[1]["reasoning_tokens"]) == (5, 3, 2)
    assert (rows[2]["prompt_tokens"], rows[2]["completion_tokens"], rows[2]["reasoning_tokens"]) == (20, 9, 4)
    assert rows[3]["reasoning_tokens"] is None and rows[3]["completion_tokens"] == 7
    fleet = lb.metrics.get("lb_reasoning_tokens_total", served_model=SERVED, node="fleet", profile="plain")
    assert fleet == 11  # 5 + 2 + 4; the null row adds nothing


# 9 ------------------------------------------------------------------------------------------------
def _fleet_equals_sum(m, name):
    groups = {}
    for lab, v in m.get(name, []):
        key = tuple(sorted((k, x) for k, x in lab.items() if k != "node"))
        groups.setdefault(key, {})[lab["node"]] = v
    assert groups, name
    for key, per in groups.items():
        fleet = per.pop("fleet")
        assert abs(fleet - sum(per.values())) < 1e-9, (name, key, fleet, per)


def test_metrics_fleet_is_sum_and_node_dropout_is_clean(two):
    lb, a, b = two
    for mode in ("json", "json,reasoning=3", "sse_chat", "json", "status=400"):
        req(lb, mode=mode, body={"stream": "sse" in mode})
    lb.pool.probe_all()
    m = metrics(lb)
    for name in ("lb_requests_total", "lb_generation_tokens_total", "lb_prompt_tokens_total", "lb_reasoning_tokens_total"):
        _fleet_equals_sum(m, name)
    for name in ("lb_node_up", "lb_requests_inflight", "lb_vllm_running", "lb_vllm_waiting"):
        _fleet_equals_sum(m, name)
    nodes = {lab["node"] for lab, _ in m["lb_vllm_generation_tokens_total"]}
    assert nodes == {"n1", "n2"}  # per node only, no fleet sum of vLLM counters
    before = {tuple(sorted(l.items())): v for l, v in m["lb_requests_total"] if l["node"] == "fleet"}
    b.stop()
    lb.pool.probe_all(); lb.pool.probe_all()
    m2 = metrics(lb)
    up = {lab["node"]: v for lab, v in m2["lb_node_up"]}
    assert up == {"n1": 1, "n2": 0, "fleet": 1}
    assert {lab["node"] for lab, _ in m2["lb_vllm_running"]} == {"n1", "fleet"}
    after = {tuple(sorted(l.items())): v for l, v in m2["lb_requests_total"] if l["node"] == "fleet"}
    for k, v in before.items():
        assert after[k] >= v  # fleet counters never go down when a node leaves
    _fleet_equals_sum(m2, "lb_requests_total")


# 10 -----------------------------------------------------------------------------------------------
def test_request_id_and_dispatch_prefix_reach_ledger_and_upstream(two):
    lb, a, b = two
    r, _ = req(lb, path="/d/bb2-abc123/v1/chat/completions", headers={"X-Request-Id": "rid-42"})
    assert r.status == 200 and r.getheader("X-Request-Id") == "rid-42"
    row = ledger(lb)[-1]
    assert row["rid"] == "rid-42" and row["dispatch_id"] == "bb2-abc123" and row["path"] == "/v1/chat/completions"
    seen = (a.seen + b.seen)[-1]
    assert seen[0] == "/v1/chat/completions" and seen[1].get("X-Request-Id") == "rid-42"
    r, _ = req(lb)
    assert re.fullmatch(r"[0-9a-f]{16}", r.getheader("X-Request-Id"))


def test_client_disconnect_is_logged(two):
    lb, a, b = two
    s = socket.create_connection(("127.0.0.1", lb.ports["plain"]))
    body = json.dumps({"stream": True}).encode()
    s.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n"
              b"X-Fake-Mode: drip=3\r\nContent-Length: %d\r\n\r\n%s" % (len(body), body))
    s.recv(200)
    s.close()
    assert wait_for(lambda: any(r["outcome"] == "client_gone" for r in ledger(lb)), 6)
