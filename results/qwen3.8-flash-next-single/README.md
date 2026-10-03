# Qwen3.8-Flash-Next single DGX Spark — 2026-09-20

This is one vLLM server on dgx01. dgx02 does not serve this deployment. The user requested
single-node measurements before considering a two-server pool.

## Recipe and lifecycle audit

Upstream: https://github.com/MiaAI-Lab/Qwen3.8-Flash-Next-Single-DGX-Spark
at `6b5086458023474a7809ea30e1bcf42f03dcd75f`.
Head checkout uses Git sparse checkout to omit restart/alert services, at derived `ad8dec9`.
Model snapshot: `925d7be6c14c6c9442ef83e8f05b5a3c39304f69`.
Mirror: https://github.com/muojp/Qwen3.8-Flash-Next-Single-DGX-Spark/tree/feat/dgx-spark-pair-ops

- Image entrypoint is `vllm serve`; no additional inference proxy.
- Upstream start.sh runs a background memory watchdog. A separately installed supervisor and
  systemd maintenance timer can relaunch the container; heartbeat/alert scripts can send host
  information to ALERT_WEBHOOK. None of those services/scripts are provisioned for this run.
- Mirror uses MEMWATCH_ENABLED=0 and explicit Docker restart=no. Startup memory budget,
  cgroup limit, graceful shutdown and necessary model patches remain.
- Required local patches: PLE mmap/offload/protocol, ModelOpt mixed quantization,
  QSA FP8 KV and reduced-vocabulary MTP. Stock Mia weights differ from the dual recipe's NVIDIA weights.
- Image pinned by digest in `.env.fleet`:
  `sha256:fc120ece0a388cc0aa1caad4a9f1cd92113484ab7ec2fd0efadd62585be05bf8`.

## Telemetry audit

The image sets VLLM_USAGE_SOURCE=production-docker-image. Its usage_lib honours
VLLM_NO_USAGE_STATS and DO_NOT_TRACK. HF constants/_telemetry honour
HF_HUB_DISABLE_TELEMETRY and offline mode. An isolated probe inside that image returned:
`vllm_usage_enabled=False, hf_telemetry_disabled=True, hf_offline=True`.

The launch and every helper container receive VLLM_NO_USAGE_STATS=1, DO_NOT_TRACK=1,
HF_HUB_DISABLE_TELEMETRY=1 and WANDB_DISABLED=true. Helpers use network=none. The serving
container also receives HF_HUB_OFFLINE=1 and TRANSFORMERS_OFFLINE=1. Download needs network
but opts out of telemetry. No webhook is configured. No stat-logger or platform plugins were
registered in the image; general plugins were only vLLM's two built-in LoRA resolvers.

Application opt-outs do not constitute an egress firewall. Local metrics remain enabled for
benchmarking. Live-container checks are recorded in `runtime-audit.json`.

## Measurement protocol

1. Thinking probe to determine honoured request key.
2. Five standard scenarios, concurrency 1/2/4/8, thinking off, unique prompts.
3. Review with thinking on and off, preserving answers for content scoring.
4. Long context: requested 115000 tokens, single and 4 concurrent, actual token count and
   head MemAvailable floor. No restart between benchmark stages.

The profile has max_num_seqs=4; c=8 includes queued requests. All results are single-node.
The long-context tool was corrected before this run: its concurrent leg used to replay the
single-leg prompt, and it only sent the GLM thinking key. New runs use unique prompts across
legs and send both thinking spellings, recording the settings. Older results are not rewritten
and may contain one prefix-cache hit in their concurrent leg.

Validation before measurement: fleet gate 80 passed; recipe image tests 14 passed, 1 skipped
(optional real NVIDIA checkpoint test, not applicable to this Mia checkpoint). The recipe tests
use small CPU tensors but require GPU visibility for VllmConfig's device inference.

## First-boot corrections

The first attempt stopped before Docker launch because the upstream log-retention pipeline used
`ls` on an empty archive under `set -e -o pipefail`. The mirror tolerates an empty first-boot
archive; a shell regression test covers it. The second attempt uses `ad8dec9`.

Two introspection helpers also mounted only the snapshot, breaking relative links into HF's
blob cache. They now mount the entire HF cache read-only and receive the full snapshot path.
Their vLLM logging level is ERROR so INFO messages cannot corrupt machine-readable results.
The model patches and vLLM serving arguments are unchanged by these fixes.

Weights: 51 files, 39 SHA256-bearing LFS files verified against the pinned manifest;
35 indexed safetensor shards, 105839538520 indexed weight bytes. See `weights-verified.json`.

## Standard throughput, thinking off

Aggregate output tokens/s (includes request wall time); one head only.

| Scenario | c=1 | c=2 | c=4 | c=8 |
|---|---:|---:|---:|---:|
| chat | 37.0 | 50.6 | 55.5 | 66.3 |
| code | 46.3 | 84.6 | 122.6 | 124.6 |
| essay | 36.4 | 60.0 | 95.2 | 91.1 |
| review | 43.7 | 61.1 | 99.4 | 81.9 |
| structured | 24.2 | 48.5 | 66.3 | 63.7 |

75/75 requests succeeded; every thinking-off row recorded zero median reasoning tokens.
The c=8 rows queue behind max_num_seqs=4. No reboot or service restart between stages.

Review, thinking off: 4/5 planted defects found in the body. The discount threshold
case was missed (0/2 signals); the other four scored 2/2. Manual inspection also found
an unsupported claim that the Laravel controller needs a Controller import even though
it is in the same namespace. The existing five-case score does not penalize extra false
positives and is not a broad quality evaluation. One C# response starts with an incorrect
namespace claim then explicitly retracts it in the same answer. Raw answers are preserved.

## Review with thinking on

15/15 requests succeeded. Aggregate tok/s at c=1/2/4/8: 41.4 / 68.6 / 93.6 / 94.7.
The standard scorer gives 4/5 + one partial (discount threshold); selected best matches are in
reasoning traces. 12/15 answers have empty bodies; 13/15 hit the 1600-token limit. Three answers
have bodies (two wallet, one expiry). Thinking-off has 0/15 empty bodies and 0/15 length finishes.
The same probe confirmed enable_thinking=false, both keys, and reasoning_effort=none suppress
reasoning; thinking=false alone is ignored. The lane uses enable_thinking=false.

## Long context and final state

Requested 115000 tokens calibrated to 162659 actual prompt tokens per request.

| Leg | Requests | Wall s | Input tok/s (end-to-end) | Head minimum MemAvailable GiB |
|---|---:|---:|---:|---:|
| single | 1 | 86.5 | 1880.2 | 15.41 |
| concurrent | 4 | 343.8 | 1892.5 | 15.36 |

The concurrent input rate is `(4 × 162659) / 343.8 = 1892.5 input tokens/s`.
This counts API `usage.prompt_tokens`, not output tokens. The denominator includes queueing,
input processing and short output generation; it is not an isolated prefill timer.
The maximum measured output-generation aggregate was **124.6 output tokens/s** (code, c=8).
Rates use unrounded elapsed time; the stored `wall_s` is rounded to one decimal.
The saved `metrics-after-bench.prom` is a cumulative server snapshot, not a before/after
measurement of this leg, so it cannot independently attribute the rate to these four requests.

Four concurrent submissions completed at 97.2 s, 343.8 s, 184.5 s, 269.9 s. Input throughput barely changes; these long inputs largely queue.
All 90 speed/review requests and all five long-context requests succeeded. No restart or cache
flush between benchmark stages. Worker memory in the raw data is idle-host context, not inference capacity.

Corrected first switch took 902 s (15m02s), including launcher preflight; vLLM readiness was 862 s after Docker launch.
The live profile allocated 16.33 GiB KV (1089581 tokens), with the 100 GiB container cap.
The head is left serving this deployment; no independent worker deployment was created.
