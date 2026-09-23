#!/usr/bin/env python3
"""dgx-lb — one entry point for a pool of OpenAI-compatible replicas (the dgx pair).

Why it exists: the Mac cannot reach dgx02 (CX7 only), and none of the bench's older proxies balance
across upstreams, retry, report health or carry request ids. Design and the host's ack:
fondi-workspace .meta/bench/bench2-20260922/PLAN-B-dgx-lb-proxy-20260924.md.

One process, several listeners ("profiles"). Every profile shares one scheduler, so in-flight counts
per node are global — two independent balancers would each think a node is free and overfill it.

    python3 dgx_lb.py --config ~/dgx-lb/config.toml

Routing   least outstanding requests, per-node cap (the replica's measured knee); when every node is
          at its cap the request waits in a bounded FIFO here instead of queueing unevenly in vLLM.
Health    GET /v1/models every `health.interval` s; a node is eligible only while it answers AND
          serves `served_model`. Connect errors / 502-504 count as failures too (passive ejection).
Retries   only before the first byte has gone to the client (connect error, 502/503/504), on the
          other node when one is eligible, with backoff. Never mid-stream, never on 4xx.
Timeouts  connect / first_byte / idle / total, by name. Startup refuses a total under 600 s or a
          first_byte under 60 s (BP-60: a 20 s "budget" once turned 14 healthy calls into 502s).
Evidence  X-Request-Id in and out, a JSONL ledger per day, usage parsed from chat JSON, chat SSE and
          Responses SSE (nulls stay null), and Prometheus /metrics with node=<name>|fleet.
"""
from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import signal
import socket
import sys
import threading
import time
import tomllib
import urllib.parse
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOP_BY_HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers",
              "transfer-encoding", "upgrade", "content-length", "host"}
RETRYABLE_STATUS = {502, 503, 504}
INJECT_PATHS = ("/v1/chat/completions", "/v1/responses", "/v1/completions")
DISPATCH_RE = re.compile(r"^/d/([A-Za-z0-9._:-]{1,128})(/.*)$")
TTFT_BUCKETS = (0.1, 0.25, 0.5, 1, 2, 5, 10, 30, 60, 120, 300, 900)
VLLM_GAUGES = {
    "running": re.compile(r'^vllm:num_requests_running\{[^}]*\}\s+(\S+)', re.M),
    "waiting": re.compile(r'^vllm:num_requests_waiting\{[^}]*\}\s+(\S+)', re.M),
    "waiting_capacity": re.compile(r'^vllm:num_requests_waiting_by_reason\{[^}]*reason="capacity"[^}]*\}\s+(\S+)', re.M),
}
VLLM_COUNTERS = {
    "generation_tokens_total": re.compile(r'^vllm:generation_tokens_total\{[^}]*\}\s+(\S+)', re.M),
    "prompt_tokens_total": re.compile(r'^vllm:prompt_tokens_total\{[^}]*\}\s+(\S+)', re.M),
}
MIN_TOTAL_S, MIN_FIRST_BYTE_S = 600, 60


# ----------------------------------------------------------------------------------------- config
DEFAULTS = {
    "listen_host": "0.0.0.0",
    "ledger_dir": "~/dgx-lb/ledger",
    "timeouts": {"connect": 5.0, "first_byte": 900.0, "idle": 300.0, "total": 3900.0},
    "queue": {"max_depth": 64},
    "health": {"interval": 5.0, "probe_timeout": 3.0, "eject_after": 2, "readmit_after": 2},
    "retry": {"max_attempts": 3, "backoff": [0.5, 1.0, 2.0]},
}


def load_config(path: str | None = None, data: dict | None = None) -> dict:
    cfg = json.loads(json.dumps(DEFAULTS))
    raw = data if data is not None else tomllib.load(open(path, "rb"))
    for k, v in raw.items():
        if isinstance(v, dict) and isinstance(cfg.get(k), dict):
            cfg[k].update(v)
        else:
            cfg[k] = v
    for need in ("served_model", "upstream", "profile"):
        if not cfg.get(need):
            raise ValueError(f"config: '{need}' is required")
    names = [u["name"] for u in cfg["upstream"]]
    if len(set(names)) != len(names) or "fleet" in names or "none" in names:
        raise ValueError("config: upstream names must be unique and not 'fleet'/'none'")
    return cfg


def check_timeouts(t: dict, allow_short: bool) -> None:
    if allow_short:
        return
    if t["total"] < MIN_TOTAL_S or t["first_byte"] < MIN_FIRST_BYTE_S:
        raise SystemExit(f"refusing timeouts total={t['total']} first_byte={t['first_byte']} (minimum "
                         f"{MIN_TOTAL_S}/{MIN_FIRST_BYTE_S} s; BP-60). Pass --i-know-short-timeouts to override.")


def deep_merge(dst: dict, src: dict) -> dict:
    """Merge src into dst, keeping keys of dst that src does not mention (unlike a top-level replace)."""
    for k, v in src.items():
        if isinstance(v, dict) and isinstance(dst.get(k), dict):
            deep_merge(dst[k], v)
        else:
            dst[k] = json.loads(json.dumps(v))
    return dst


# ---------------------------------------------------------------------------------------- metrics
def _fmt_labels(labels: dict) -> str:
    if not labels:
        return ""
    inner = ",".join(f'{k}="{str(v).replace(chr(92), chr(92) * 2).replace(chr(34), chr(92) + chr(34))}"'
                     for k, v in sorted(labels.items()))
    return "{" + inner + "}"


class Metrics:
    """Tiny Prometheus registry. Every LB-own counter is incremented for its node AND for node="fleet"
    in the same call, so the fleet series is monotonic even when a node disappears."""

    def __init__(self):
        self.lock = threading.Lock()
        self.counters: dict[tuple[str, tuple], float] = {}
        self.hist: dict[tuple, list] = {}  # key -> [bucket counts..., count, sum]
        self.help = {}

    def inc(self, name: str, labels: dict, value: float = 1.0, fleet: bool = True) -> None:
        with self.lock:
            for lab in ([labels, {**labels, "node": "fleet"}] if fleet else [labels]):
                key = (name, tuple(sorted(lab.items())))
                self.counters[key] = self.counters.get(key, 0.0) + value

    def observe(self, name: str, labels: dict, value: float) -> None:
        with self.lock:
            for lab in (labels, {**labels, "node": "fleet"}):
                key = (name, tuple(sorted(lab.items())))
                h = self.hist.setdefault(key, [0] * len(TTFT_BUCKETS) + [0, 0.0])
                for i, b in enumerate(TTFT_BUCKETS):
                    if value <= b:
                        h[i] += 1
                h[-2] += 1
                h[-1] += value

    def get(self, name: str, **labels) -> float:
        with self.lock:
            return self.counters.get((name, tuple(sorted(labels.items()))), 0.0)

    def render_counters(self) -> list[str]:
        out = []
        with self.lock:
            for name in sorted({k[0] for k in self.counters}):
                out.append(f"# TYPE {name} counter")
                for (n, lab), v in sorted(self.counters.items()):
                    if n == name:
                        out.append(f"{name}{_fmt_labels(dict(lab))} {v:g}")
            for name in sorted({k[0] for k in self.hist}):
                out.append(f"# TYPE {name} histogram")
                for (n, lab), h in sorted(self.hist.items()):
                    if n != name:
                        continue
                    d = dict(lab)
                    for i, b in enumerate(TTFT_BUCKETS):
                        out.append(f"{name}_bucket{_fmt_labels({**d, 'le': f'{b:g}'})} {h[i]}")
                    out.append(f"{name}_bucket{_fmt_labels({**d, 'le': '+Inf'})} {h[-2]}")
                    out.append(f"{name}_count{_fmt_labels(d)} {h[-2]}")
                    out.append(f"{name}_sum{_fmt_labels(d)} {h[-1]:g}")
        return out


# ------------------------------------------------------------------------------------------ nodes
class Node:
    def __init__(self, name: str, url: str, cap: int):
        u = urllib.parse.urlsplit(url)
        self.name, self.url, self.cap = name, url.rstrip("/"), int(cap)
        self.host, self.port = u.hostname, u.port or 80
        self.inflight = 0
        self.up = False
        self.fails = 0          # consecutive failures (probe or passive)
        self.goods = 0          # consecutive good probes while down
        self.last_probe = None  # {"ts","ok","reason","served"}
        self.vllm = {}          # last scraped gauges + counters
        self.last_pick = 0.0


class Pool:
    """Scheduler + health state. All node fields are guarded by `cond`."""

    def __init__(self, cfg: dict, metrics: Metrics):
        self.cfg, self.metrics = cfg, metrics
        self.served = cfg["served_model"]
        self.nodes = [Node(u["name"], u["url"], u.get("cap", 4)) for u in cfg["upstream"]]
        self.cond = threading.Condition()
        self.queue_depth = 0
        self.max_depth = int(cfg["queue"]["max_depth"])
        h = cfg["health"]
        self.eject_after, self.readmit_after = int(h["eject_after"]), int(h["readmit_after"])

    # --- health -------------------------------------------------------------------------------
    def _mark(self, node: Node, ok: bool, reason: str, hard: bool = False) -> None:
        with self.cond:
            if ok:
                node.fails = 0
                if not node.up:
                    node.goods += 1
                    if node.goods >= self.readmit_after:
                        node.up, node.goods = True, 0
            else:
                node.goods = 0
                node.fails += 1
                if hard or node.fails >= self.eject_after:
                    node.up = False
            self.cond.notify_all()

    def passive_success(self, node: Node) -> None:
        with self.cond:
            node.fails = 0

    def passive_failure(self, node: Node, reason: str) -> None:
        self.metrics.inc("lb_upstream_errors_total", {"served_model": self.served, "node": node.name, "kind": reason})
        self._mark(node, False, reason)

    def probe(self, node: Node) -> None:
        t = float(self.cfg["health"]["probe_timeout"])
        served, reason, ok = None, "ok", False
        try:
            with urllib.request.urlopen(f"{node.url}/v1/models", timeout=t) as r:
                served = [m.get("id") for m in json.load(r).get("data", [])]
            ok = self.served in served
            reason = "ok" if ok else "wrong_model"
        except Exception as e:  # noqa: BLE001 — any failure is a failed probe
            reason = type(e).__name__
        first = node.last_probe is None
        node.last_probe = {"ts": time.time(), "ok": ok, "reason": reason, "served": served}
        if ok and first:
            with self.cond:
                node.up, node.fails, node.goods = True, 0, 0
                self.cond.notify_all()
        else:
            self._mark(node, ok, reason, hard=(reason == "wrong_model"))
        if ok:
            try:
                with urllib.request.urlopen(f"{node.url}/metrics", timeout=t) as r:
                    text = r.read().decode("utf-8", "replace")
                vals = {}
                for k, pat in {**VLLM_GAUGES, **VLLM_COUNTERS}.items():
                    found = [float(x) for x in pat.findall(text)]
                    vals[k] = sum(found) if found else None
                node.vllm = vals
            except Exception:  # noqa: BLE001
                node.vllm = {}
        else:
            node.vllm = {}

    def probe_all(self) -> None:
        threads = [threading.Thread(target=self.probe, args=(n,), daemon=True) for n in self.nodes]
        for th in threads:
            th.start()
        for th in threads:
            th.join()

    # --- scheduling ---------------------------------------------------------------------------
    def acquire(self, avoid: set[str], deadline: float) -> tuple[Node | None, str, float]:
        """Return (node, outcome, queue_wait_s). outcome is 'ok', 'queue_full', 'no_upstream' or 'timeout_first_byte'."""
        t0 = time.time()
        with self.cond:
            queued = False
            try:
                while True:
                    eligible = [n for n in self.nodes if n.up]
                    if not eligible:
                        return None, "no_upstream", time.time() - t0
                    preferred = [n for n in eligible if n.name not in avoid] or eligible
                    free = [n for n in preferred if n.inflight < n.cap]
                    if free:
                        node = min(free, key=lambda n: (n.inflight, n.last_pick))
                        node.inflight += 1
                        node.last_pick = time.time()
                        return node, "ok", time.time() - t0
                    if not queued:
                        if self.queue_depth >= self.max_depth:
                            return None, "queue_full", 0.0
                        self.queue_depth += 1
                        queued = True
                    left = deadline - time.time()
                    if left <= 0:
                        return None, "timeout_first_byte", time.time() - t0
                    self.cond.wait(timeout=min(left, 1.0))
            finally:
                if queued:
                    self.queue_depth -= 1

    def release(self, node: Node) -> None:
        with self.cond:
            node.inflight -= 1
            self.cond.notify_all()


# ---------------------------------------------------------------------------------------- usage
class UsageTap:
    """Watches response bytes and keeps the last usage block seen (JSON body or SSE events)."""
    MAX_JSON = 8 << 20

    def __init__(self, content_type: str):
        ct = (content_type or "").lower()
        self.sse = "text/event-stream" in ct
        self.json = "json" in ct and not self.sse
        self.buf = bytearray()
        self.usage = None
        self.saw_done = False

    def feed(self, chunk: bytes) -> None:
        if self.json:
            if len(self.buf) < self.MAX_JSON:
                self.buf += chunk
            return
        if not self.sse:
            return
        self.buf += chunk
        while True:
            i = self.buf.find(b"\n")
            if i < 0:
                break
            line = bytes(self.buf[:i]).strip()
            del self.buf[:i + 1]
            if not line.startswith(b"data:"):
                continue
            data = line[5:].strip()
            if data == b"[DONE]":
                self.saw_done = True
                continue
            try:
                ev = json.loads(data)
            except ValueError:
                continue
            self._take(ev)

    def _take(self, ev) -> None:
        if not isinstance(ev, dict):
            return
        if isinstance(ev.get("usage"), dict):
            self.usage = ev["usage"]
        if ev.get("type") in ("response.completed", "response.incomplete", "response.failed"):
            self.saw_done = True
            resp = ev.get("response") or {}
            if isinstance(resp.get("usage"), dict):
                self.usage = resp["usage"]

    def finish(self) -> dict:
        if self.json and self.buf:
            try:
                self._take(json.loads(bytes(self.buf)))
            except ValueError:
                pass
        return normalise_usage(self.usage)


def normalise_usage(u: dict | None) -> dict:
    if not isinstance(u, dict):
        return {"prompt_tokens": None, "completion_tokens": None, "reasoning_tokens": None}

    def first(*vals):
        for v in vals:
            if v is not None:
                return v
        return None
    ctd = u.get("completion_tokens_details") or {}
    otd = u.get("output_tokens_details") or {}
    return {"prompt_tokens": first(u.get("prompt_tokens"), u.get("input_tokens")),
            "completion_tokens": first(u.get("completion_tokens"), u.get("output_tokens")),
            "reasoning_tokens": first(ctd.get("reasoning_tokens"), otd.get("reasoning_tokens"), u.get("reasoning_tokens"))}


# ----------------------------------------------------------------------------------------- ledger
class Ledger:
    def __init__(self, directory: str):
        self.dir = os.path.expanduser(directory)
        os.makedirs(self.dir, exist_ok=True)
        self.lock = threading.Lock()

    def write(self, row: dict) -> None:
        path = os.path.join(self.dir, time.strftime("ledger-%Y%m%d.jsonl", time.gmtime()))
        line = json.dumps(row, ensure_ascii=False) + "\n"
        with self.lock, open(path, "a") as f:
            f.write(line)


# ---------------------------------------------------------------------------------------- handler
class ClientGone(Exception):
    pass


def make_handler(lb: "LB", profile: dict):
    inject = profile.get("inject") or {}
    pname = profile["name"]

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "dgx-lb"

        def log_message(self, fmt, *args):  # the ledger is the log
            pass

        # -- local endpoints ------------------------------------------------------------------
        def _send_json(self, code: int, obj: dict, rid: str | None = None) -> None:
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            if rid:
                self.send_header("X-Request-Id", rid)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            if path == "/health":
                st = lb.health_state()
                return self._send_json(200 if st["eligible"] else 503, st)
            if path == "/metrics":
                body = lb.render_metrics().encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; version=0.0.4")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            m = DISPATCH_RE.match(path)
            if (m.group(2) if m else path) == "/v1/models":
                if not any(n.up for n in lb.pool.nodes):
                    return self._send_json(503, {"error": f"no upstream serves {lb.served}"})
                return self._send_json(200, {"object": "list", "data": [
                    {"id": lb.served, "object": "model", "owned_by": "dgx-lb"}]})
            return self._proxy(b"")

        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n) if n else b""
            return self._proxy(body)

        # -- proxying -------------------------------------------------------------------------
        def _proxy(self, body: bytes):
            t0 = time.time()
            to = lb.timeouts
            deadline_total = t0 + to["total"]
            rid = (self.headers.get("X-Request-Id") or "").strip()[:128] or uuid.uuid4().hex[:16]
            path = self.path
            dispatch_id = None
            m = DISPATCH_RE.match(path)
            if m:
                dispatch_id, path = m.group(1), m.group(2)
            stream = None
            if self.command == "POST" and inject and path.split("?")[0] in INJECT_PATHS:
                try:
                    obj = json.loads(body)
                    if isinstance(obj, dict):
                        stream = bool(obj.get("stream"))
                        body = json.dumps(deep_merge(obj, inject)).encode()
                except ValueError:
                    pass
            elif self.command == "POST":
                try:
                    stream = bool(json.loads(body).get("stream"))
                except (ValueError, AttributeError):
                    pass
            row = {"ts": round(t0, 3), "rid": rid, "profile": pname, "dispatch_id": dispatch_id,
                   "method": self.command, "path": path, "stream": stream, "node": None, "attempts": 0,
                   "status": None, "outcome": None, "queue_wait_s": 0.0, "ttft_s": None, "wall_s": None,
                   "bytes": 0, "prompt_tokens": None, "completion_tokens": None, "reasoning_tokens": None,
                   "errors": []}
            fwd_headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_BY_HOP}
            fwd_headers["X-Request-Id"] = rid
            fwd_headers["Content-Length"] = str(len(body))
            try:
                self._run(body, path, fwd_headers, row, t0, deadline_total)
            finally:
                row["wall_s"] = round(time.time() - t0, 3)
                lb.finish(row)

        def _run(self, body, path, fwd_headers, row, t0, deadline_total):
            to = lb.timeouts
            avoid: set[str] = set()
            attempts_max = int(lb.cfg["retry"]["max_attempts"])
            backoff = list(lb.cfg["retry"]["backoff"])
            for attempt in range(1, attempts_max + 1):
                row["attempts"] = attempt
                fb_deadline = min(deadline_total, t0 + to["first_byte"])
                node, why, waited = lb.pool.acquire(avoid, fb_deadline)
                row["queue_wait_s"] = round(row["queue_wait_s"] + waited, 3)
                if node is None:
                    row["outcome"] = why
                    code = 504 if why == "timeout_first_byte" else 503
                    row["status"] = code
                    self._send_json(code, {"error": why, "request_id": row["rid"]}, row["rid"])
                    return
                row["node"] = node.name
                try:
                    result = self._attempt(node, body, path, fwd_headers, row, t0, fb_deadline, deadline_total)
                finally:
                    lb.pool.release(node)
                if result == "done":
                    return
                # retryable failure before any byte reached the client
                avoid.add(node.name)
                lb.metrics.inc("lb_retries_total", {"served_model": lb.served, "node": node.name, "profile": pname})
                if attempt < attempts_max:
                    pause = backoff[min(attempt - 1, len(backoff) - 1)]
                    if time.time() + pause >= fb_deadline:
                        break
                    time.sleep(pause)
            if row["outcome"] is None or row["outcome"] == "retrying":
                row["outcome"] = "upstream_error"
            row["status"] = 502
            self._send_json(502, {"error": "upstream_error", "detail": row["errors"][-3:], "request_id": row["rid"]}, row["rid"])

        def _attempt(self, node, body, path, fwd_headers, row, t0, fb_deadline, deadline_total) -> str:
            to = lb.timeouts
            conn = http.client.HTTPConnection(node.host, node.port, timeout=to["connect"])
            try:
                try:
                    conn.connect()
                    conn.sock.settimeout(max(0.05, fb_deadline - time.time()))
                    conn.request(self.command, path, body=body if self.command == "POST" else None, headers=fwd_headers)
                    resp = conn.getresponse()
                except (socket.timeout, TimeoutError) as e:
                    if time.time() >= fb_deadline - 0.01:
                        row["errors"].append(f"{node.name}: first_byte timeout")
                        row["outcome"], row["status"] = "timeout_first_byte", 504
                        lb.pool.passive_failure(node, "timeout_first_byte")
                        self._send_json(504, {"error": "timeout_first_byte", "request_id": row["rid"]}, row["rid"])
                        return "done"
                    row["errors"].append(f"{node.name}: {type(e).__name__}")
                    lb.pool.passive_failure(node, "connect")
                    row["outcome"] = "retrying"
                    return "retry"
                except OSError as e:
                    row["errors"].append(f"{node.name}: {type(e).__name__}: {e}")
                    lb.pool.passive_failure(node, "connect")
                    row["outcome"] = "retrying"
                    return "retry"
                if resp.status in RETRYABLE_STATUS:
                    try:
                        resp.read()
                    except Exception:  # noqa: BLE001
                        pass
                    row["errors"].append(f"{node.name}: HTTP {resp.status}")
                    lb.pool.passive_failure(node, f"http_{resp.status}")
                    row["outcome"] = "retrying"
                    return "retry"
                # From here the client gets this response, whatever happens.
                lb.pool.passive_success(node)
                row["status"] = resp.status
                ttft = time.time() - t0
                row["ttft_s"] = round(ttft, 3)
                lb.metrics.observe("lb_ttft_seconds", {"served_model": lb.served, "node": node.name}, ttft)
                tap = UsageTap(resp.getheader("Content-Type", ""))
                clen = resp.getheader("Content-Length")
                self.send_response(resp.status)
                for k, v in resp.getheaders():
                    if k.lower() not in HOP_BY_HOP:
                        self.send_header(k, v)
                self.send_header("X-Request-Id", row["rid"])
                self.send_header("X-Dgx-Lb-Node", node.name)
                chunked = clen is None
                if chunked:
                    self.send_header("Transfer-Encoding", "chunked")
                else:
                    self.send_header("Content-Length", clen)
                self.end_headers()
                outcome = "ok" if row["attempts"] == 1 else "retried_ok"
                try:
                    while True:
                        left = deadline_total - time.time()
                        if left <= 0:
                            outcome = "timeout_total"
                            break
                        conn.sock.settimeout(min(to["idle"], left))
                        try:
                            chunk = resp.read1(65536)
                        except (socket.timeout, TimeoutError):
                            outcome = "timeout_total" if time.time() >= deadline_total - 0.01 else "timeout_idle"
                            break
                        except (http.client.IncompleteRead, OSError, http.client.HTTPException) as e:
                            row["errors"].append(f"{node.name}: midstream {type(e).__name__}")
                            outcome = "midstream_broken"
                            break
                        if not chunk:
                            break
                        tap.feed(chunk)
                        row["bytes"] += len(chunk)
                        try:
                            if chunked:
                                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                            else:
                                self.wfile.write(chunk)
                            self.wfile.flush()
                        except OSError:
                            raise ClientGone()
                    if outcome == "ok" or outcome == "retried_ok":
                        if clen is not None and row["bytes"] < int(clen):
                            outcome = "midstream_broken"
                    if chunked:
                        try:
                            self.wfile.write(b"0\r\n\r\n")
                            self.wfile.flush()
                        except OSError:
                            raise ClientGone()
                    if outcome not in ("ok", "retried_ok"):
                        self.close_connection = True
                except ClientGone:
                    outcome = "client_gone"
                    self.close_connection = True
                row["outcome"] = outcome
                row.update(tap.finish())
                return "done"
            finally:
                conn.close()

    return Handler


# ------------------------------------------------------------------------------------------- LB
class _Server(ThreadingHTTPServer):
    request_queue_size = 128   # the stdlib default of 5 drops SYNs when several lanes burst at once
    daemon_threads = True


class LB:
    def __init__(self, cfg: dict, allow_short: bool = False):
        self.cfg = cfg
        self.served = cfg["served_model"]
        self.timeouts = {k: float(v) for k, v in cfg["timeouts"].items()}
        check_timeouts(self.timeouts, allow_short)
        self.metrics = Metrics()
        self.pool = Pool(cfg, self.metrics)
        self.ledger = Ledger(cfg["ledger_dir"])
        self.servers: list[ThreadingHTTPServer] = []
        self.ports: dict[str, int] = {}
        self._stop = threading.Event()
        self.started = time.time()

    def finish(self, row: dict) -> None:
        node = row["node"] or "none"
        base = {"served_model": self.served, "node": node, "profile": row["profile"]}
        self.metrics.inc("lb_requests_total", {**base, "outcome": row["outcome"] or "unknown"})
        for k in ("prompt_tokens", "completion_tokens", "reasoning_tokens"):
            if isinstance(row.get(k), (int, float)):
                name = {"prompt_tokens": "lb_prompt_tokens_total", "completion_tokens": "lb_generation_tokens_total",
                        "reasoning_tokens": "lb_reasoning_tokens_total"}[k]
                self.metrics.inc(name, base, float(row[k]))
        self.ledger.write(row)

    def health_state(self) -> dict:
        with self.pool.cond:
            nodes = {n.name: {"up": n.up, "inflight": n.inflight, "cap": n.cap, "fails": n.fails,
                              "last_probe": n.last_probe, "url": n.url} for n in self.pool.nodes}
            depth = self.pool.queue_depth
        return {"served_model": self.served, "eligible": sum(1 for v in nodes.values() if v["up"]),
                "nodes": nodes, "queue_depth": depth, "uptime_s": round(time.time() - self.started, 1)}

    def render_metrics(self) -> str:
        lines = self.metrics.render_counters()
        g = []
        with self.pool.cond:
            snap = [(n.name, n.up, n.inflight, n.cap, dict(n.vllm)) for n in self.pool.nodes]
            depth = self.pool.queue_depth
        sm = self.served

        def gauge(name, rows):
            g.append(f"# TYPE {name} gauge")
            for lab, v in rows:
                g.append(f"{name}{_fmt_labels({'served_model': sm, **lab})} {v:g}")

        gauge("lb_node_up", [({"node": n}, 1 if up else 0) for n, up, *_ in snap] +
              [({"node": "fleet"}, sum(1 for _, up, *__ in snap if up))])
        gauge("lb_requests_inflight", [({"node": n}, inf) for n, _, inf, *_ in snap] +
              [({"node": "fleet"}, sum(s[2] for s in snap))])
        gauge("lb_node_cap", [({"node": n}, cap) for n, _, _, cap, _ in snap] +
              [({"node": "fleet"}, sum(s[3] for s in snap if s[1]))])
        gauge("lb_queue_depth", [({"node": "fleet"}, depth)])
        for key in VLLM_GAUGES:
            rows = [({"node": n}, v[key]) for n, up, _, _, v in snap if up and v.get(key) is not None]
            gauge(f"lb_vllm_{key}", rows + [({"node": "fleet"}, sum(r[1] for r in rows))])
        for key in VLLM_COUNTERS:  # per node only: a replica restart resets them, so no fleet sum here
            rows = [({"node": n}, v[key]) for n, up, _, _, v in snap if up and v.get(key) is not None]
            g.append(f"# TYPE lb_vllm_{key} counter")
            for lab, v in rows:
                g.append(f"lb_vllm_{key}{_fmt_labels({'served_model': sm, **lab})} {v:g}")
        return "\n".join(lines + g) + "\n"

    def _health_loop(self) -> None:
        interval = float(self.cfg["health"]["interval"])
        while not self._stop.wait(interval):
            self.pool.probe_all()

    def start(self) -> None:
        self.pool.probe_all()
        for prof in self.cfg["profile"]:
            srv = _Server((self.cfg["listen_host"], int(prof["port"])), make_handler(self, prof))
            srv.daemon_threads = True
            self.servers.append(srv)
            self.ports[prof["name"]] = srv.server_address[1]
            threading.Thread(target=srv.serve_forever, daemon=True, name=f"lb-{prof['name']}").start()
        threading.Thread(target=self._health_loop, daemon=True, name="lb-health").start()

    def stop(self) -> None:
        self._stop.set()
        for s in self.servers:
            s.shutdown()
            s.server_close()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", required=True)
    ap.add_argument("--i-know-short-timeouts", action="store_true")
    args = ap.parse_args()
    cfg = load_config(args.config)
    lb = LB(cfg, allow_short=args.i_know_short_timeouts)
    lb.start()
    print(json.dumps({"event": "started", "ports": lb.ports, "served_model": lb.served,
                      "nodes": {n.name: n.up for n in lb.pool.nodes}, "timeouts": lb.timeouts}), flush=True)
    done = threading.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda *_: done.set())
    done.wait()
    lb.stop()
    print(json.dumps({"event": "stopped"}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
