# dgx-lb

One entry point for a pool of OpenAI-compatible replicas on the dgx pair. The first user is
`qwen3.8-flash-next-single-x2`: two independent qs-single replicas, one per Spark. dgx02 is reachable
only over CX7, so the bench lanes on the Mac go through dgx-lb on dgx01. Design and ack:
fondi-workspace `.meta/bench/bench2-20260922/PLAN-B-dgx-lb-proxy-20260924.md`.

Python 3.11+ standard library only. Files:
- `dgx_lb.py`: the service. The module docstring explains routing, health, retries and timeouts.
- `config.qs-x2.toml`: the config for the qs pair (install it as `~/dgx-lb/config.toml`).
- `dgx-lb.service`: the systemd user unit. The install lines are in its header.
- `test_dgx_lb.py`: fixture tests with fake vLLM upstreams. Run `pytest -q tools/dgx-lb`.

## Endpoints (every profile port serves them all)
| port | profile | body |
|---|---|---|
| 8890 | plain | unchanged |
| 8891 | think | `chat_template_kwargs.enable_thinking=true` merged in; other kwargs are kept |
| 8892 | nothink | `chat_template_kwargs.enable_thinking=false` merged in |

- `/v1/...`: proxied. `/d/<dispatch_id>/v1/...` is proxied the same way, and the id is recorded (bench I5 attribution).
- `/v1/models`: the configured served name while at least one node is eligible, otherwise 503.
- `/health`: node states, in-flight counts and queue depth. It returns 200 while at least one node is eligible, otherwise 503.
- `/metrics`: Prometheus text. Labels are `served_model`, `node` (`dgx01`, `dgx02`, `fleet`, and `none` for requests that never reached a node), and `profile`.
  - LB counters (`lb_requests_total{outcome}`, `lb_*_tokens_total`, `lb_retries_total`, `lb_upstream_errors_total`, `lb_ttft_seconds`) have a monotonic `fleet` series equal to the sum of the per-node series.
  - Gauges (`lb_node_up`, `lb_requests_inflight`, `lb_node_cap`, `lb_queue_depth`, `lb_vllm_running|waiting|waiting_capacity`) have `fleet` = the sum over the nodes that are up.
  - `lb_vllm_*_tokens_total` are each replica's own vLLM counters. They are per node only, because a replica restart resets them.
  - Fleet tok/s: `sum by (served_model) (rate(lb_vllm_generation_tokens_total[1m]))`.

## Security
dgx-lb binds the same interfaces as vLLM's 8888 and has no auth, the same VPN/LAN-only convention (host ruling 2026-09-24). Do not expose these ports beyond that network.

## Ledger
`~/dgx-lb/ledger/ledger-YYYYMMDD.jsonl` gets one row per request with these fields: rid (`X-Request-Id`, taken from the client or generated), profile, dispatch_id, path, stream, node, attempts, status, outcome, queue_wait_s, ttft_s, wall_s, bytes, prompt/completion/reasoning tokens, errors.

`outcome` is one of: ok, retried_ok, upstream_error, midstream_broken, client_gone, timeout_first_byte, timeout_idle, timeout_total, queue_full, no_upstream.

A usage value the server did not report stays null. For example, qs reports `reasoning_tokens` only on non-streamed responses.
