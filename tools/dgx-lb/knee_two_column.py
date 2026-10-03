#!/usr/bin/env python3
"""Knee table in the operator's two-column form (OPERATOR-INSTRUCTION-20260924-1021): per level, the aggregate
tok/s and its gain x vs c=1 next to the per-session numbers (decode tok/s median [p5], wall/ETA p50 [p95], TTFT p95)
and their loss x vs c=1. Optional node scrape (scrape_nodes*.ldjson) adds capacity waits and KV usage for the leg.

    knee_two_column.py QUICKBENCH.json [--scrape SCRAPE.ldjson --node dgx01] [--md]

Flags per level: NO VALUE = aggregate gain < +20% over the previous level while the per-session wall is > 1.5x c=1.
Knee (operator rule + host guards): the largest c whose aggregate gain over the previous level is >= +20%, whose
TTFT p95 <= max(2 x c1, 1.0 s) when the prompt is short (long prompts: reported, not applied - prefill queueing is
what is being measured), and that is not NO VALUE. Capacity waits are reported per leg (the scrape is per leg).
Rows from quickbench versions before per_request lack p5/p95 per session; those cells print '-'."""
import json, sys

args = sys.argv[1:]
q = json.load(open(args[0]))
md = "--md" in args
scr = None
if "--scrape" in args:
    node = args[args.index("--node") + 1] if "--node" in args else "dgx01"
    s = [json.loads(l) for l in open(args[args.index("--scrape") + 1])]
    g = [x["nodes"][node] for x in s if node in x.get("nodes", {})]
    scr = {"max_running": max((x.get("running") or 0) for x in g), "max_waiting_capacity": max((x.get("waiting_capacity") or 0) for x in g),
           "max_kv_cache_usage": round(max((x.get("kv_cache_usage_perc") or 0) for x in g), 3),
           "preemptions": (g[-1].get("preemptions_total") or 0) - (g[0].get("preemptions_total") or 0)}

for think in sorted({r["thinking"] for r in q["rows"]}, reverse=True):
    rows = sorted((r for r in q["rows"] if r["thinking"] == think), key=lambda r: r["concurrency"])
    base = rows[0]
    out, prev, knee = [], None, None
    for r in rows:
        agg_x = r["aggregate_tok_s"] / base["aggregate_tok_s"]
        step = (r["aggregate_tok_s"] / prev["aggregate_tok_s"] - 1) if prev else None
        dec_x = (r["per_stream_tok_s"] / base["per_stream_tok_s"]) if base.get("per_stream_tok_s") and r.get("per_stream_tok_s") else None
        wall_x = r["latency_p50_s"] / base["latency_p50_s"]
        novalue = step is not None and step < 0.20 and wall_x > 1.5
        short = (r.get("prompt_tokens") or 0) < 2000
        ttft_ok = r["ttft_p95_s"] <= max(2 * base["ttft_p95_s"], 1.0)
        if prev is None or (step >= 0.20 and not novalue and (ttft_ok or not short)):
            knee = r["concurrency"]
        out.append({"c": r["concurrency"], "agg": r["aggregate_tok_s"], "agg_x": round(agg_x, 2),
                    "step_pct": None if step is None else round(100 * step), "dec_p50": r.get("per_stream_tok_s"),
                    "dec_p5": r.get("per_stream_tok_s_p5"), "dec_x": None if dec_x is None else round(dec_x, 2),
                    "wall_p50": r["latency_p50_s"], "wall_p95": r.get("latency_p95_s"), "wall_x": round(wall_x, 2),
                    "ttft_p95": r["ttft_p95_s"], "ratio": round(agg_x / wall_x, 2), "flag": "NO VALUE" if novalue else ""})
        prev = r
    head = f"{q.get('note','')} | think={think} | prompt~{base.get('prompt_tokens')} tok | knee (operator rule) = {knee}"
    if md:
        print(f"\n**{head}**\n")
        print("| c | aggregate tok/s | agg × c1 | step | per-session decode p50 [p5] | decode × c1 | wall p50 [p95] s | wall × c1 | TTFT p95 s | agg×/wall× | flag |")
        print("|---:|---:|---:|---:|---|---:|---|---:|---:|---:|---|")
        for o in out:
            st = "—" if o["step_pct"] is None else f"{o['step_pct']:+d}%"
            print(f"| {o['c']} | {o['agg']} | {o['agg_x']} | {st} | {o['dec_p50']} [{o['dec_p5'] if o['dec_p5'] is not None else '-'}] | "
                  f"{o['dec_x']} | {o['wall_p50']} [{o['wall_p95'] if o['wall_p95'] is not None else '-'}] | {o['wall_x']} | {o['ttft_p95']} | {o['ratio']} | {o['flag']} |")
        if scr:
            print(f"\nscrape: {json.dumps(scr)}")
    else:
        print(json.dumps({"head": head, "rows": out, "scrape": scr, "knee": knee}))
