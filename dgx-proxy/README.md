# dgx-proxy

The one entry point for the dgx pair's inference servers. Clients use a single base URL whatever
deployment is running, and the request's `model` picks the deployment. The proxy load-balances
deployments that run as several replicas (qs-x2 runs one replica per Spark), and it treats a TP=2
pair deployment as a single backend: only dgx01 answers for it, so it falls out of the health probe.
Per-deployment **rewrite rules** (data, not code) absorb client/server API mismatches. One example
is codex sending `include` and non-function tools to the GLM TensorFold server, which rejects both.

It is a Rust (tokio + hyper 1) rewrite and generalisation of [`tools/dgx-lb`](../tools/dgx-lb/).

## How it differs from dgx-lb
| | dgx-lb (Python) | dgx-proxy |
|---|---|---|
| model | one fixed `served_model`; a backend serving anything else is `wrong_model` and ejected | any number of `[[deployment]]`s; each backend's `/v1/models` answer decides which deployment it serves, so a `dgx-model` switch needs no restart and no config change |
| routing | every request to the one pool | by `model` (served name or alias); an unknown model goes to the only eligible deployment (left unchanged), or gets a 404 that lists the models when more than one is eligible |
| body edits | profile `inject` (merge) only | rules `remove` / `filter_array` / `merge` / `set_default` / `rename_model`, per profile and per deployment; alias → served name is automatic |
| profiles | one listener port each (8890/8891/8892) | the same ports (optional) **and** the path prefix `/p/<profile>/v1/...` on any port |
| concurrency | a thread per connection | async; SSE frames are passed on as they arrive (nothing is buffered) |
| cap | per upstream | per deployment, applied to each backend serving it |

Unchanged from dgx-lb: least-outstanding with a per-backend cap and a bounded FIFO when every backend is full; in-flight counts shared by all ports and profiles; active and passive health with `eject_after`/`readmit_after`; retries only before the first byte, on another backend, with backoff (never mid-stream, never on 4xx); the four named timeouts and the BP-60 startup floor; `X-Request-Id`; the `/d/<dispatch_id>/v1/...` prefix; the usage parsing rules; the metric names.

## Build and run
dgx01 has no Rust toolchain yet. The design's choice is a user-level `rustup` on dgx01 and a native build there (no sudo):
```sh
rsync -a --exclude target dgx-proxy/ dgx01:~/src/dgx-proxy/
ssh dgx01 'cd ~/src/dgx-proxy && cargo build --release && cargo test'
```
Then install the binary as `~/dgx-proxy/dgx-proxy` and the config as `~/dgx-proxy/config.toml`; the install lines are in the header of `dgx-proxy.service`.

```
dgx-proxy --config <path> [--i-know-short-timeouts] [--check]
```
`--check` validates the config and exits; the unit runs it as `ExecStartPre`. `--i-know-short-timeouts` lifts the BP-60 floor (total ≥ 600 s, first_byte ≥ 60 s) and is for tests and dev only.

Files: `src/` (lib + `main.rs`), `tests/` (fake upstreams in-process, `cargo test`), `config.example.toml` (every key, commented), `dgx-proxy.service` (systemd user unit).

## Config
`config.example.toml` is the reference. In short:
- `listen` (default `0.0.0.0:8880`), `ledger_dir`, `[timeouts]`, `[health]`, `[queue] max_depth`, `[retry]`: same meaning as in dgx-lb.
- `[[backend]] name, url`: a server port. Which deployment a backend serves is not configured.
- `[[deployment]] name, served_models, aliases, cap, rewrites`: names match `deployments/*.toml`. `cap` applies to each backend.
- `[[rule]] name, paths, op, ...`: `paths` lists the exact upstream paths it applies to, matched after the prefixes are stripped and without the query. Pointers are RFC 6901 JSON pointers.
  - `remove`: `pointer`.
  - `filter_array`: `pointer`, `keep_where = { "/sub/pointer" = value, ... }`. An element is kept only when every subpointer equals its value.
  - `merge`: `value` (a table), optional `pointer`. A deep merge: keys the value does not mention are kept.
  - `set_default`: `pointer`, `value`. Applied only when the pointer is absent.
  - `rename_model`: `to`, optional `from = [...]`.
- `[[profile]] name, port (optional), rewrites`: a request on the profile's port, or under `/p/<name>/`, gets the profile's rules. Everything else uses the implicit profile `default`, which has no rules.

Order of rewrites: the profile's rules, then the deployment's rules, then the alias rename. A body is buffered (up to 128 MiB; 413 above) only for `POST`/`PUT`/`PATCH` under `/v1/` (routing needs `model`) or on a path some rule names. When no rule changes it, the original bytes are forwarded unchanged. Any other request body is streamed through, and is then not retried.

## Endpoints (on every port)
- `/v1/...`: proxied. `/d/<dispatch_id>/v1/...` and `/p/<profile>/v1/...` are proxied the same way (in either order); the ledger records the dispatch id and the profile.
- `/v1/models` (`GET`, with or without prefixes): the served names and aliases of every eligible deployment. It returns 503 when no deployment is eligible.
- `/health`: JSON with each backend (up, fails, in-flight, the models it reports, the deployments it serves, the last probe) and each deployment (eligible backends, in-flight, queue depth, rewrites). It returns 200 while at least one deployment is eligible, otherwise 503.
- `/metrics`: Prometheus text.

Responses carry `X-Request-Id` (taken from the client or generated as 16 hex chars, and also sent upstream) and `X-Dgx-Lb-Node`. Local errors are JSON `{"error": <outcome>, "request_id": ...}`. The 404 for an ambiguous model also carries `models`.

## Metrics
dgx-lb's names and labels are kept so the existing Grafana panels keep working. A `deployment` label is added. `served_model` is the deployment's first served name, or `none` for a request that matched no deployment.
- Counters: `lb_requests_total{outcome}`, `lb_prompt_tokens_total`, `lb_generation_tokens_total`, `lb_reasoning_tokens_total`, `lb_retries_total`, `lb_upstream_errors_total{kind}`, and the `lb_ttft_seconds` histogram (first body bytes). Labels: `served_model`, `deployment`, `node` (a backend name, `none` for a request that never reached one, and `fleet`), and `profile`. The `fleet` series is incremented together with the node series, so it equals their sum and never goes down when a node leaves.
- New: `lb_rewrites_total{rule,deployment,profile}`. It counts the rules that changed a body, and the automatic alias rename appears as `rule="rename_model"`.
- Gauges per deployment × backend: `lb_node_up` (1 = up and serving this deployment), `lb_requests_inflight`, `lb_node_cap`. The `fleet` value is the sum over the backends (for `lb_node_cap`, over the serving backends only). `lb_queue_depth{node="fleet"}` is reported per deployment.
- Backend scrape: after each good probe the backend's `/metrics` is read for `vllm:num_requests_running|waiting|waiting_by_reason{reason="capacity"}` (gauges `lb_vllm_running|waiting|waiting_capacity`, with a fleet sum) and `vllm:generation_tokens_total|prompt_tokens_total` (counters `lb_vllm_*_tokens_total`, per node only). **Follow-up:** TensorFold's own `/metrics` format is not mapped yet. The design asks for per-backend-type patterns, and a TensorFold backend currently yields no `lb_vllm_*` series.

## Ledger
`<ledger_dir>/ledger-YYYYMMDD.jsonl` (UTC) gets one row per proxied request. The fields are dgx-lb's (`ts, rid, profile, dispatch_id, method, path, stream, node, attempts, status, outcome, queue_wait_s, headers_s, ttft_s, wall_s, bytes, prompt_tokens, completion_tokens, reasoning_tokens, errors`) plus `deployment`, `model_in`, `model_out` and `rewrites` (the names of the rules that changed the body). Request and response bodies are never written. A usage value the server did not report stays null.

`outcome` is dgx-lb's vocabulary: ok, retried_ok, upstream_error, midstream_broken, client_gone, timeout_first_byte, timeout_idle, timeout_total, queue_full, no_upstream. Three local outcomes are added: `unknown_model` (404, an ambiguous model), `body_too_large` (413), and `bad_route` (404, an unknown `/p/<profile>`).

`client_gone` covers a client that disconnects at any point: while queued, while waiting for headers, or mid-stream. In every case the upstream connection is closed right away, which matches TensorFold's client-gone poll.

## Security
Like dgx-lb, the proxy binds the same interfaces as the servers' 8888 and has no auth: VPN/LAN only. The config holds no secrets.

## Rollout (see the design's §8; every live step is a page decision)
1. Run `dgx-proxy` on **8880** beside `dgx-lb.service`, with no profile ports, and point no clients at it yet. Check GLM through it with curl: SSE, `/health`, `/metrics`.
2. The first client is the voxel run's codex: set `[model_providers.dgx].base_url = http://<dgx01>:8880/v1`. This replaces `strip_include_proxy.py`.
3. Port takeover: once qs-x2 has been checked through `/p/{plain,think,nothink}`, stop `dgx-lb.service`, uncomment `port = 8890/8891/8892` in the profiles, and restart `dgx-proxy`. To roll back, restore the units.
4. Other clients, one at a time: IFR's `IFR_DGX_BASE_URL`, the bench lanes, and the terminus dgx lane.
