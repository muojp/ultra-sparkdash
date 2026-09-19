# AGENTS.md

How to work on this repository: reaching the machines, adding a model, and measuring one. Written
for whoever arrives next, human or agent. Nothing here relies on memory of a past session — if a
step is missing, it is missing, and the fix is to add it here.

Related files: `deployments/README.md` is the deployment procedure in detail, `TASKS.md` is the
current working state, `tests/README.md` explains the gate.

---

## 1. The machines

Two DGX Spark (GB10, aarch64, 121.7 GiB unified memory each). **Host RAM is GPU memory** — every
`cudaMalloc` is committed host memory, and the kernel OOM killer cannot see the driver's
allocations, so overcommitting wedges the node rather than killing a process.

| | address | notes |
|---|---|---|
| head | `muo@192.168.0.100` (`dgx01`) | everything is driven from here |
| worker | `dgx02` from the head; `192.168.100.40` over CX7 | **not reachable from the Mac LAN**; its Wi-Fi address is not routable, WireGuard `10.10.10.22` is |
| fabric | ConnectX-7, `enp1s0f0np0` / `rocep1s0f0`, RoCEv2 **GID index 3** | confirm GIDs per node under `/sys/class/infiniband/<dev>/ports/1/gids/`; a wrong index hangs `ncclCommInitRank` **silently** |

```sh
ssh muo@192.168.0.100            # head
ssh muo@192.168.0.100 'ssh dgx02 hostname'
```

Two things bite immediately on this kit:

- `~/.cache/huggingface/hub` on the head is **root-owned** (an old sudo download). Set
  `HF_HOME=~/hf-home` (or another user-owned path) for anything that fetches weights.
- Port **8888** is shared by every deployment, so they are mutually exclusive. A start must follow
  a stop; `dgx-model switch` does that in the right order, a hand-run `./start.sh` does not.

`dgx-model` re-executes itself on the head over ssh for `list` / `status` / `switch`, and stays
local for `bench` / `longctx` / `sync`. The head keeps its **own copy** of this tool under
`~/dgx-model` — a plain directory, not a checkout — so a new deployment file is invisible there
until `bin/dgx-model sync`.

---

## 2. Registering a model

Every candidate is a *deployment*: one `deployments/<name>.toml`, one recipe submodule, one entry
in the fleet. Deployments are named after **whose recipe** it is, not the model: three of them serve
GLM-5.3-Flash, so `glm-5.3-flash` alone is ambiguous and the tests reject it.

### 2.1 Mirror the recipe — always, never pin upstream directly

```sh
gh repo fork <UPSTREAM_OWNER>/<REPO> --clone=false     # creates muojp/<REPO>
git submodule add https://github.com/muojp/<REPO>.git <REPO>
cd <REPO>
git remote add upstream https://github.com/<UPSTREAM_OWNER>/<REPO>.git
git fetch upstream
git checkout -B feat/dgx-spark-pair-ops "$(git rev-parse upstream/main)"
git push -u origin feat/dgx-spark-pair-ops
```

Why: this kit is not the recipe author's kit, and fixes accumulate. A submodule pinned at upstream
has nowhere to put them, so the next person re-derives them or edits the checkout on the head where
nothing records it. Every adaptation becomes a derived commit on that branch, one concern each.

Worked examples, all real:

| what upstream got wrong *for us* | fix |
|---|---|
| NFS exporter left `/export` on the container's own overlay2 root, which the kernel nfsd refuses | bind a host dir at `/export`, add `crossmnt` (`07f307b` in the EXL3 mirror) |
| `config.json` optional in staging, required by the loader — every rank died after NCCL came up | fetch it, and fail in staging where the message can still name the directory (`b56cddc`) |
| `HF_HUB_DISABLE_XET=1` (our own workaround) made ~95 GiB files unfetchable | force Xet for that one call, keep an escape hatch (`00621d5`) |
| vLLM posts a hardware fingerprint to `stats.vllm.ai` on every boot | `VLLM_NO_USAGE_STATS=1` + `DO_NOT_TRACK=1` in the compose/env |

### 2.2 Write the deployment file

Start from an existing one. Required: `name` (= file stem, enforced by a test), `served_model`,
`api_url`, `boot_timeout_s`, `[host]` with `recipe_dir` and the recipe's **own** start/stop/status
commands — never reimplement them — and either `compose_project` or a `[containers]` table.

Record what is *not* done yet in `[prerequisites]`, and every trap you hit in
`[prerequisites.traps]`. A deployment may be defined long before it can run: `switch` refuses a
target whose `recipe_dir` is missing **before** stopping whatever is serving, because finding out
afterwards costs a boot to undo.

Special shapes that already exist, as precedent:

- **single-node recipe, run as two servers** (`qwen3.8-27b-sglang`): `api_urls` lists both
  endpoints, `bench_from = "head"` puts the client where both are equally reachable.
- **licence-constrained engine** (`glm-5.3-flash-miaai`): the recipe defaults to DFlash2, which we
  may not use; the file pins `SPEC_METHOD=mtp` and says so as a trap, because the recipe's entire
  benchmark table is DFlash2's and does not apply to us.
- **a recipe that does not fit the pattern** (`glm-5.3-flash-bizuayeu`): a Python kit whose
  `server start` supervises in the foreground. The file says so and carries wrappers to verify by
  hand before anyone trusts a switch.

### 2.3 Provision and boot

```sh
bin/dgx-model sync                      # the head has its own copy; without this it does not exist
bin/dgx-model list
bin/dgx-model switch <name> --dry-run   # prints the exact recipe commands
bin/dgx-model switch <name>
```

Fetch images and weights **before** the first boot, and download once: pull to the head and rsync
to the worker over CX7 (~100 s for 29 GiB) rather than fetching twice from Hugging Face.

**Check weights with `stat -L`, never with `du` or a recipe's own check.** huggingface_hub keeps
blobs in a store shared across repos (`hub/blobs/<xx>/<sha>`), so a model directory is mostly
symlinks into it. Every recipe here judges by that directory and every one of them has been
confidently wrong in both directions: "Nothing to do" with 123 GiB present, "Weights present on
both nodes" with the worker holding none of it, and an rsync of the model directory that copies
symlinks and no data. The failures surface far away — a gloo rendezvous error, or a tokenizer the
server cannot read and blames on a missing sentencepiece. Resolve the links and copy the blobs:

```sh
for f in "$D"/snapshots/*/*; do t=$(readlink -f "$f") || continue
  case "$t" in "$HUB"/blobs/*) echo "${t#$HUB/}" ;; esac
done | sort -u > /tmp/blobs.txt
rsync -a --files-from=/tmp/blobs.txt "$HUB/" "worker:$HUB/"
```

**Boot budgets, measured 2026-09-18/19.** A switch is stop + start + wait-until-serving; the first
boot of a deployment also pays for an image pull and, where the engine compiles, for that too.

| deployment | warm switch | first boot |
|---|---|---|
| `deepseek-v4-flash` | 7m03s | — |
| `deepseek-v4.1-flash` | 8m58s (9m31s on another run) | +33 min of image pull before it |
| `glm-5.3-flash-himorishige` | 11m49s | — |
| `glm-5.3-flash-miaai` | 7m59s | — |
| `qwen3.8-27b-sglang` | — | 27m12s: ~20 min on the head (torch.compile) then the worker, because the two servers start in sequence |

Two things those numbers hide. The image pull is the long pole on a cold host — the EXL3 deployment
spent 33 minutes there before the engine started, one layer of it 11 minutes behind registry
retries — so pull images while something else is measuring. And a pair deployment's *first* boot
also copies weights to the worker; budget that separately (123 GiB took 6 minutes over CX7 at
355 MB/s, against hours from Hugging Face).

Expect the first boot of a new deployment to fail on something. Both EXL3 failures were recipe
defects that only appear on a kit other than the author's, and each surfaced minutes after the step
that caused it. Budget for it, fix it on the mirror, write the trap down.

---

## 3. Benchmarking — speed

```sh
bin/dgx-model bench -- --scenario all -c 1,2,4,8 --no-thinking --note "why this run is special"
bin/dgx-model longctx -- --tokens 115000 -c 4 --no-thinking
bin/llm-bench-report                    # -> results/report.html
```

`bench` resolves the API and model from whatever is serving, refuses if that deployment is not the
one answering, and writes a row file under `~/.local/state/dgx-model/bench/` named for the
deployment — attribution is the reason to go through `dgx-model` rather than calling the tool
directly.

What the numbers mean, and what quietly breaks them:

- **Aggregate tok/s is the pair's work; per-stream is what one caller feels.** They move in opposite
  directions as concurrency rises, and reporting only one of them hides the trade.
- **Prompts are unique per request by default.** A shared prompt is answered from the prefix cache
  and reports throughput the server cannot sustain (`--shared-prefix` measures that path on
  purpose).
- **Scenarios matter more than the headline number.** These recipes ship two or three speculative
  engines and say the ranking flips with the workload; measured here, GLM-5.3 ran 8.4 tok/s on
  `chat` and 18.0 on `review` in the same conditions. Use `--scenario all` and compare like with
  like.
- **The memory floor is reached during a long prefill**, never at boot and never under steady
  decode. `longctx` reads it per node from Prometheus. A 3 GiB KV pool boots, passes a smoke test,
  and then dies at ~470k of a 600k prompt.
- **Tokenizers disagree**: the probe calibrates chars-per-token per model with one short request.
  A fixed ratio once made "the same" 115k prompt 325k tokens on another model.
- **Do not reboot to make the numbers look alike.** A deployment that only performs well on a
  freshly booted server does not perform well, because the review lane will not restart it between
  requests. Measure it as it has been used. When GLM-5.3 returned 5.9 tok/s on a `chat` sweep run
  after a long-context leg, against 18.9 on a fresh one, that gap is the result — its prefix cache
  was holding 32% of the KV pool and the sparse indexer scans the prefix. Record the sequence a
  run followed, not a restart that hides it.
- **Reasoning is a dimension, not a footnote.** The same model with thinking on and off is two
  systems for a review lane: `--thinking-modes on,off` (and `--reasoning-effort` where the server
  takes it) produces separate rows labelled `review/think` and `review/nothink`, and the report
  keys on them so neither replaces the other. Comparing two deployments means holding this equal —
  GLM-5.3 found every planted defect with thinking on and returned an empty message body doing it.
- **Note the conditions.** `--note` is recorded with the run and shown in the report, so a number
  taken during a weight download stays readable as one instead of being compared as if it matched.

The report accumulates: every run is kept, the comparison table shows the latest per cell, and the
layout does not change as runs are added.

---

## 4. Benchmarking — content

Speed without quality is not a result. The pair's main job is **code review**, so the review
scenario carries real bug-fix cases: `bench/review_cases/cases.json`, five short files with exactly
one planted defect each, across PHP/Laravel and C#, covering a plain logic mistake, a check-then-act
race, and input reaching a sink unvalidated.

Each case carries `expected` — the finding written out — so an answer can be scored without
re-deriving the bug.

**The answer key must never reach the model.** `id` names the defect (`laravel-balance-race`),
`defect` is its category, `expected` is the finding itself. `review_prompt` sends only the language
and the code, asks the neutral question a reviewer is asked, and runs `case_leak()` over what it is
about to send — a leak raises rather than producing a number. The instruction deliberately does
*not* list the defect categories: enumerating them turns "find the problem" into a three-way
multiple choice.

Adding a case: append to `cases.json` with `id`, `language`, `defect`, `expected`, `code`, then
`make check` — the tests require the three defect kinds and both stacks to stay represented, and
verify no case's answer key shows through in its prompt.

Scoring is manual for now: run the scenario, read the completions against `expected`, and record
what fraction of cases each deployment finds. That gap — fast but wrong versus slower and right —
is the one that decides what the review lane runs, and no throughput table shows it.

---

### What the review lane should send

`[lane].extra` in each deployment file is the request body the lane merges in — in practice the
reasoning setting, e.g. `extra = { chat_template_kwargs = { thinking = false } }`. The key is
always present, `{}` when nothing should be sent, because an absent key and an empty one read the
same downstream and only one of them means "measured". `make check` enforces that.

Do not infer the value from the model family: of the three GLM-5.3 deployments here, one honours
the kwarg, one has never been asked, and the fastest deployment in the fleet ignores it while
reporting 16,376 reasoning tokens in runs that asked for thinking off. Probe the deployment that is
serving, in one request, and read `reasoning_content` rather than trusting the flag:

```sh
curl -s http://192.168.0.100:8888/v1/chat/completions -H 'content-type: application/json' -d '{
  "model": "<served_model>", "max_tokens": 200, "temperature": 0,
  "messages": [{"role": "user", "content": "Reply with the single word: ready."}],
  "chat_template_kwargs": {"thinking": false}}' |
  python3 -c 'import json,sys; m=json.load(sys.stdin)["choices"][0]["message"]; print("body:", len(m.get("content") or ""), "reasoning:", len(m.get("reasoning_content") or ""))'
```

A 400 means the template does not define the variable (send nothing); reasoning with a non-empty
length means it was accepted and ignored. `results/review-scoring.md` records what each deployment
did and, more usefully, whether its findings land in the body at all.

## 5. The gate

```sh
make check      # pytest + every deployment file parses + every script parses
```

**Passes before a tool change is committed, and before a measurement taken with a changed tool is
trusted.** Every bug it covers was silent in the output rather than loud: a fixed token ratio
comparing two models on different work, a shared prompt inflating throughput from cache, an empty
container list taking down every command that calls `status()` mid-switch, and the head's containers
being looked for on the operator's laptop.
