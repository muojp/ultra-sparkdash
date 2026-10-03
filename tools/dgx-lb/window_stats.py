#!/usr/bin/env python3
"""Window stats from fleet_scrape.py samples: LB queue depth, in-flight, vLLM running/capacity waits per node + fleet,
generation tok/s from each replica's counter. Usage: window_stats.py SCRAPE.ldjson [--minutes 15]"""
import json, sys
m = 15.0
if "--minutes" in sys.argv:
    m = float(sys.argv[sys.argv.index("--minutes") + 1])
rows = [json.loads(l) for l in open(sys.argv[1])]
rows = [r for r in rows if r.get("series")]
end = rows[-1]["t"]; w = [r for r in rows if r["t"] >= end - m * 60]
def col(k): return sorted(r["series"].get(k, 0.0) for r in w)
def p(v, q): return v[min(len(v) - 1, int(q * (len(v) - 1) + 0.5))] if v else None
out = {"window_min": m, "samples": len(w)}
for k in ("lb_queue_depth|node=fleet", "lb_requests_inflight|node=fleet", "lb_vllm_running|node=fleet",
          "lb_vllm_waiting_capacity|node=fleet", "lb_vllm_waiting_capacity|node=dgx01", "lb_vllm_waiting_capacity|node=dgx02"):
    v = col(k); out[k] = {"p50": p(v, .5), "p95": p(v, .95), "max": v[-1] if v else None, "nonzero_frac": round(sum(x > 0 for x in v) / len(v), 2) if v else None}
span = w[-1]["t"] - w[0]["t"]
for n in ("dgx01", "dgx02"):
    k = f"lb_vllm_generation_tokens_total|node={n}"
    if k in w[0]["series"] and k in w[-1]["series"] and span > 0:
        out[f"gen_tok_s_{n}"] = round((w[-1]["series"][k] - w[0]["series"][k]) / span, 1)
out["gen_tok_s_fleet"] = round(sum(v for k, v in out.items() if k.startswith("gen_tok_s_dgx")), 1)
print(json.dumps(out))
