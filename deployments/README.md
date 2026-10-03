# deployments/ — what the pair can serve, and how a new one gets there

The 2× DGX Spark pair holds **one** 300B-class checkpoint at a time, so every candidate is a
*deployment*: one `<name>.toml` here, one recipe submodule, one entry in `bin/dgx-model`.
The proposed topology and single/replicated selection design is in
[`docs/dgx-model-topology-design.md`](../docs/dgx-model-topology-design.md) (not implemented).

This file is the procedure. Nothing about it lives in anyone's head or in an assistant's memory —
if a step is missing here, it is missing.

Not every deployment is a pair deployment. `qwen3.8-27b-sglang` serves a 27B model from a single
GB10 through SGLang, and it is here for the same reason the others are: it takes port 8888, so it
is mutually exclusive with them and belongs in the same switch. What differs — one node instead of
two, `sglang:*` metrics instead of `vllm:*` — is written in its own file rather than assumed.

| deployment | recipe submodule | shape |
|---|---|---|
| `deepseek-v4-flash` | `DeepSeek-v4-Flash-DSpark-2x-DGX-Spark` | Compose project `deepseek-v4-flash` |
| `glm-5.3-flash-himorishige` | `glm53-flash-2x-dgx-spark-recipe` | Compose project `glm53` |
| `deepseek-v4.1-flash` | `DeepSeek-v4.1-Flash-EXL3-2x-DGX-Sparks` | no Compose: `docker run` + named containers |
| `qwen3.8-27b-sglang` | `Qwen3.8-27B-SGLang-DGX-Spark` | single node ×2, SGLang, `docker run` (not provisioned) |
| `glm-5.3-flash-miaai` | `GLM-5.3-Flash-EXL3-2x-DGX-Sparks` | EXL3, MTP only — DFlash2 is licence-blocked (not provisioned) |
| `glm-5.3-flash-bizuayeu` | `GLM-5.3-Flash-NVFP4-2x-DGX-Sparks-BIZ` | Python kit, foreground supervisor (not provisioned) |

Deployments are named after **whose recipe** they are, not after the model, because three of them
serve GLM-5.3-Flash and two serve a DeepSeek V4 family member. `glm-5.3-flash` on its own is
ambiguous and the tests reject it.

## 1. The mirror rule

**Every recipe is a submodule of a `muojp` mirror, never of upstream directly**, and every change we
need lives as a *derived commit* on a branch of that mirror (`feat/dgx-spark-pair-ops` by
convention). Upstream stays a second remote (`upstream`) for pulling their work in.

Why: this kit is not the recipe author's kit. Fixes and adaptations accumulate, and a submodule
pinned at upstream has nowhere to put them — the next person then re-derives them from scratch,
or worse, edits the checkout on the head where nothing records it.

```sh
gh repo fork <UPSTREAM_OWNER>/<REPO> --clone=false          # once, creates muojp/<REPO>
cd <submodule>
git remote set-url origin https://github.com/muojp/<REPO>.git
git remote add upstream https://github.com/<UPSTREAM_OWNER>/<REPO>.git
git checkout -B feat/dgx-spark-pair-ops <upstream commit>   # derive from a named upstream commit
# … commit the fixes, one concern per commit, each message saying what it reproduces …
git push -u origin feat/dgx-spark-pair-ops
cd .. && git add <submodule> .gitmodules                    # pins the derived commit
```

Derived commits so far:

- `DeepSeek-v4.1-Flash-EXL3-2x-DGX-Sparks` `62ca673` — `download.sh` exited 1 with **no output at
  all** when `MODEL_HOST` did not exist yet (`set -e` + `pipefail` on `find | wc -l`, before the
  first echo). `start.sh`'s `count_model_shards` already ended in `|| true`; the fix gives
  `download.sh` the same guard. Only bites when the weights live outside the recipe's `./model`.

## 2. Adding a deployment

1. Add the mirror as a submodule (§1), pinned at the derived commit.
2. Write `deployments/<name>.toml`. Required: `served_model` (what `/v1/models` answers with),
   `api_url`, `boot_timeout_s`, `[host]` (`recipe_dir` on the head, `worker` ssh alias, and the
   recipe's own `start` / `stop` / `status` commands — never reimplement them), and `[lane]`
   hints for the review lane. Detection is either `compose_project` **or** `[containers]`
   (`head` / `worker` name lists) for a recipe that does not use Compose.
3. Say what is not done yet in `[prerequisites]`. A deployment may be defined long before it can
   run; `dgx-model switch` refuses a target whose `recipe_dir` is absent **before** it stops
   whatever is serving, because finding out afterwards costs a 15–25 minute boot to undo.
4. `bin/dgx-model sync`. The head runs its **own copy** of this tool under `~/dgx-model` — a plain
   directory, not a checkout — and `list` / `status` / `switch` re-execute there over ssh. Until the
   new file is copied across, the deployment does not exist as far as any of them is concerned, and
   the symptom is simply that `list` does not show it.
5. `bin/dgx-model list` and `status` should show it immediately. `switch <name> --dry-run` prints
   the exact recipe commands it would run, and a target whose `recipe_dir` is missing on the head is
   refused *before* anything is stopped.

## 3. Provisioning one (the EXL3 deployment, 2026-09-18)

```sh
# on the head (dgx01)
git clone https://github.com/muojp/<REPO>.git ~/<REPO> && cd ~/<REPO>
git checkout <the commit the submodule pins>        # same revision as the mirror, not "main"
cp .env.example .env                                 # then edit for this kit, see below
mkdir -p "$MODEL_HOST" "$ENGRAM_DIR"                 # before any download (§4 trap 1)
tmux new -d -s <name>-dl '~/<name>-download.sh'      # resumable; log under <recipe>/logs/
```

This kit's values, which differ from every recipe's defaults: head `192.168.100.30`,
worker `192.168.100.40`, both CX7 ports `enp1s0f0np0` + `rocep1s0f0`, RoCEv2 GID index `3`
(verified per node under `/sys/class/infiniband/<dev>/ports/1/gids/`), worker user `muo`,
weights under `~/models/`. Confirm the GIDs per node rather than copying them: the index is
per-NIC, and the wrong one hangs `ncclCommInitRank` with no error.

A download of this size runs while the *other* model is serving production traffic, so start the
GLM recipe's page-cache flusher alongside it (`glm53-cluster/scripts/cache-flusher.sh watch <secs>`):
the GB10 driver does not reclaim page cache by itself.

What the EXL3 run actually took (2026-09-18, for the next estimate): 49 minutes for the 201 GiB
EXL3 tree at ~4.4 GiB/min, five `peer closed connection` errors that all resumed by themselves, then
46 minutes for the 189 GiB Engram pair over Xet at ~4 GiB/min. Roughly 1.6 hours and 390 GiB of
disk for one deployment, with GLM serving throughout. `./download.sh` is the completeness check —
re-run it and read the last line (`complete.`) rather than counting files by hand.

The first boot then cost about as much again. Budget ~25 minutes for the image pull alone on a
cold host (one layer took 11 minutes behind registry retries), and expect the first start to fail
on something no later run will hit: traps 6 and 7 below were both found this way, each one
surfacing several minutes after the step that actually caused it. Once the image is local and
those are fixed, a switch to this deployment takes **9m30s** end to end — 1m40s to stop the other
model and stage the export, 7 minutes to load 39 shards, then KV cache, CUDA graphs and the
recipe's own warmup ladder.

## 4. Traps that cost time here

1. **`download.sh` silent exit** — fixed in the mirror (§1), but any recipe with `set -e` and a
   `find` over a not-yet-created directory has the same shape. If a download script exits 1 and
   prints nothing, look for that before anything else.
2. **`~/.cache/huggingface/hub` is root-owned on dgx01** (an earlier sudo download). Metadata
   writes are refused with a `Permission denied` warning; `--local-dir` downloads still work. Set
   `HF_HUB_CACHE` to a path this user owns rather than chowning root state.
3. **Xet hangs on this host** (`huggingface_hub` 1.20.1 era, `glm53-cluster/scripts/download-nvfp4.sh`).
   `HF_HUB_DISABLE_XET=1` was carried into the EXL3 `.env` for the same reason — see trap 4 for
   what that then cost. Re-test the hang before copying the flag into a new kit: on
   `hf-xet 1.6.0` / `huggingface_hub 1.32.0` it did not reproduce.
4. **A file over ~50 GiB needs Xet, so trap 3's workaround breaks it.** The Engram shards are
   ~95 GiB each and the regular path refuses them outright (`too large to be downloaded using the
   regular download method`). It fails *instantly* and *after* the 197 GiB EXL3 fetch has already
   succeeded, which reads like a late failure but is a settings conflict. The mirror's
   `download.sh` (`00621d5`) forces Xet for that one call; `ENGRAM_FORCE_XET=0` restores the old
   behaviour. Xet's chunk cache stayed at 5 MiB, so it needs no second copy of the weights.
5. **Port 8888 is shared by every deployment.** The first start after a provisioning run must
   follow a stop; `dgx-model switch` does that in the right order, a hand-run `./start.sh` does not.
6. **The NFS exporter cannot export its own container root on overlay2.** `exportfs: /export does
   not support NFS export` → the entrypoint dies under `set -e` and the container restart-loops,
   surfacing as "NFS server did not become ready". A recipe written on a kit whose Docker storage
   driver hands each container a real dataset (zfs, btrfs) will not have met this. Fixed in the
   mirror (`07f307b`) by binding a host directory at `/export` — and with `crossmnt`, without
   which the worker mounts the export and sees an **empty directory** instead of an error.
7. **A file the loader requires can be "optional" in the staging step.** `config.json` was missing
   from `ENGRAM_FILES`, and `prepare_engram_src.py` copied it only `if present`. The download
   reported success, the weights staged, NCCL came up, and *then* every rank died with
   `FileNotFoundError: /engram-src/config.json`. Fixed in the mirror (`b56cddc`): fetch it, and
   fail in the staging step where the message can still name the directory to fix.

## 5. Switching

```sh
bin/dgx-model status                 # which model answers /v1/models right now
bin/dgx-model switch <name> --dry-run
bin/dgx-model switch <name>          # stop the other, drop page cache, start, wait for /v1/models
bin/dgx-model history
```

The the review lane is switched separately, with the hints in `[lane]`:
`fondi-workspace/tools/lane-review-rs/deploy/dgx-switch-model.py` pauses the lane, drains it,
calls `dgx-model switch`, applies the model/timeout/pool/price and resumes.

## 6. Observability — what a new deployment gets for free, and what it has to carry

Automatic, no per-model configuration:

- **Prometheus** scrapes one host and two ports for the pair, not one per model:
  `192.168.0.100:5555` (sparkDash's own exporter) and `192.168.0.100:9106`
  (`observability/vllm-metrics/vllm_metrics_proxy.py`, which re-serves the inference server's
  `/metrics` verbatim). Every deployment serves on `:8888` on the same head, so the proxy follows
  the switch. Nothing in `prometheus.yml` names a model.
- **Grafana**: the `model` template variable is `label_values(vllm:generation_tokens_total,
  model_name)`, so a new model appears in the picker the moment it serves a token. The Deployment
  row (active model + a per-model serving timeline) reads `sparkdash_llm_available` by model.
- **sparkDash** probes `/v1/models` and the vLLM metrics, and keys its restart-safe lifetime totals
  on `spark:port:model` — a switched-out model keeps its own series instead of inheriting the next
  one's (that inheritance was a real incident on 2026-09-18).

Per-recipe, and therefore a derived commit when upstream lacks it:

- **`--enable-prompt-tokens-details`.** vLLM leaves `usage.prompt_tokens_details` null unless asked,
  so an OpenAI-compatible client records `cached_tokens: 0` however well the prefix cache works.
  All three recipes now pass it: the DeepSeek V4 compose has it upstream, GLM-5.3 got it in
  `glm53-flash-2x-dgx-spark-recipe` 2a964a5, and EXL3 in the mirror's `9bfcb5c`
  (`PROMPT_TOKENS_DETAILS=0` turns it off for a base image whose vLLM predates the flag).
  The Prometheus prefix-cache series comes from vLLM's own metrics either way; this flag is what
  makes a *client* able to tell a hit from a miss.
- **TokenTrace Live** is DeepSeek-V4-Flash only. It reads that recipe's expert-routing recorder
  (43 layers × 256 experts, MTP-5), which the GLM and EXL3 stacks do not produce, so those panels
  stay dark while they serve. That is expected, not a fault to chase.

After a switch, the honest check is `bin/dgx-model status` (which model answers `/v1/models`) plus
the Deployment row's timeline — the gap in it is the loading window, and a model that never fills
the gap did not boot.
