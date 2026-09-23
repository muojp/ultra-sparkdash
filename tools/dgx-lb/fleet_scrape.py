#!/usr/bin/env python3
"""Sample a dgx-lb /metrics endpoint every INTERVAL s and append one JSON line per sample with the lb_*
series (per node + fleet). Probe/soak scripts read fleet and per-node numbers from this one source.
Usage: fleet_scrape.py URL OUT.ldjson [INTERVAL]   (stops on SIGTERM/SIGINT)"""
import json, re, signal, sys, time, urllib.request

url, out = sys.argv[1], sys.argv[2]
interval = float(sys.argv[3]) if len(sys.argv) > 3 else 5.0
stop = False
for s in (signal.SIGTERM, signal.SIGINT):
    signal.signal(s, lambda *_: globals().__setitem__("stop", True))
LINE = re.compile(r'^(lb_[a-z_]+)(\{[^}]*\})?\s+(\S+)$')
with open(out, "a") as f:
    while not stop:
        t = time.time()
        row = {"t": round(t, 3), "series": {}}
        try:
            text = urllib.request.urlopen(url, timeout=3).read().decode()
            for line in text.splitlines():
                m = LINE.match(line)
                if not m or m.group(1).endswith("_bucket"):
                    continue
                labels = dict(re.findall(r'(\w+)="([^"]*)"', m.group(2) or ""))
                key = m.group(1) + "|" + ",".join(f"{k}={v}" for k, v in sorted(labels.items()) if k != "served_model")
                row["series"][key] = float(m.group(3))
        except Exception as e:  # noqa: BLE001
            row["error"] = type(e).__name__
        f.write(json.dumps(row) + "\n"); f.flush()
        time.sleep(max(0.0, interval - (time.time() - t)))
