#!/usr/bin/env python3
"""Summarise a soak through dgx-lb from fleet_scrape.py samples (+ the quickbench JSON when given).
Usage: soak_report.py LB_SCRAPE.ldjson [QUICKBENCH.json] [--baseline TOKS]
Prints per node and fleet: requests by outcome, retries, upstream errors, generation tok/s (from each
replica's vLLM counter as proxied by the LB), peak in-flight / vLLM running / capacity waits."""
import json, sys

args = [a for a in sys.argv[1:] if not a.startswith("--")]
baseline = None
if "--baseline" in sys.argv:
    baseline = float(sys.argv[sys.argv.index("--baseline") + 1])
rows = [json.loads(l) for l in open(args[0])]
rows = [r for r in rows if r.get("series")]
first, last = rows[0]["series"], rows[-1]["series"]
span = rows[-1]["t"] - rows[0]["t"]


def delta(prefix):
    out = {}
    for k, v in last.items():
        if k.startswith(prefix + "|"):
            out[k.split("|", 1)[1]] = v - first.get(k, 0.0)
    return out


def peak(prefix):
    out = {}
    for r in rows:
        for k, v in r["series"].items():
            if k.startswith(prefix + "|"):
                lab = k.split("|", 1)[1]
                out[lab] = max(out.get(lab, 0.0), v)
    return out


rep = {"samples": len(rows), "span_s": round(span, 1)}
rep["requests"] = {k: v for k, v in delta("lb_requests_total").items() if v}
rep["retries"] = {k: v for k, v in delta("lb_retries_total").items() if v}
rep["upstream_errors"] = {k: v for k, v in delta("lb_upstream_errors_total").items() if v}
gen = delta("lb_vllm_generation_tokens_total")
rep["vllm_generation_tokens"] = gen
nodes = {k: v for k, v in gen.items()}
rep["gen_tok_s_mean"] = {k: round(v / span, 1) for k, v in nodes.items()}
rep["gen_tok_s_mean"]["fleet(sum)"] = round(sum(nodes.values()) / span, 1)
pk = {}
for a, b in zip(rows, rows[1:]):
    dt = b["t"] - a["t"]
    tot = 0.0
    for k in gen:
        key = "lb_vllm_generation_tokens_total|" + k
        if key in a["series"] and key in b["series"]:
            d = (b["series"][key] - a["series"][key]) / dt
            pk[k] = max(pk.get(k, 0.0), d)
            tot += d
    pk["fleet(sum)"] = max(pk.get("fleet(sum)", 0.0), tot)
rep["gen_tok_s_peak5s"] = {k: round(v, 1) for k, v in pk.items()}
for name in ("lb_requests_inflight", "lb_vllm_running", "lb_vllm_waiting", "lb_vllm_waiting_capacity", "lb_queue_depth"):
    rep["peak_" + name] = peak(name)
rep["node_up_min"] = {k: min(r["series"].get("lb_node_up|" + k, 0) for r in rows) for k in ("node=dgx01", "node=dgx02")}
if len(args) > 1:
    q = json.load(open(args[1]))
    r0 = q["rows"][0]
    rep["quickbench"] = {k: r0.get(k) for k in ("concurrency", "requests", "failed", "aggregate_tok_s", "per_stream_tok_s",
                                                  "ttft_p50_s", "ttft_p95_s", "latency_p50_s", "endpoints")}
    if baseline:
        rep["vs_baseline_pct"] = round(100 * (r0["aggregate_tok_s"] / baseline - 1), 1)
print(json.dumps(rep, indent=1))
