# TASKS

Working state for the dgx pair, kept here rather than in a chat log or anyone's head. Same rule as
`deployments/README.md`: if a step is missing here, it is missing. Numbers live in the deployment
files; this file only says what is done, what is running, and what is next.

_Last updated: 2026-09-19 05:45Z_

## The gate

`make check` — pytest plus a parse of every deployment file and script — **passes before a tool
change is committed, and before a measurement taken with a changed tool is trusted.** See
`tests/README.md` for why: every bug it has caught so far was silent in the output.

## Now — six of seven measured

| deployment | decode (5 scenarios) | long context | review content |
|---|---|---|---|
| `glm-5.3-flash-himorishige` | done | done | 5/5, every finding in the reasoning trace |
| `deepseek-v4-flash` | done | done | answers captured, unscored |
| `deepseek-v4.1-flash` | done | done | answers captured, unscored |
| `qwen3.8-27b-sglang` | done (pool and single node) | **redo** | answers captured, unscored |
| `glm-5.3-flash-miaai` | done | done (2 of 4 long requests dropped) | answers captured, unscored |
| `qwen3.8-flash-next` | done — fastest everywhere | done | answers captured, unscored |
| `glm-5.3-flash-bizuayeu` | **blocked** | — | — |

- [ ] **bizuayeu is blocked in preflight.** `glm53_setup/server.py:204` hardcodes
      `Path.home()/".cache/huggingface"` and ignores HF_HOME, so it cannot find the snapshot we
      keep in `~/hf-home` (this head's hub directory is root-owned). Fix it on the mirror the way
      the sibling recipe took WORKER_HF_HOME, then run the pass.
- [ ] **Re-run the Qwen 27B long-context leg.** Its two attempts are void: one replayed a cached
      prompt (fixed seeds — 140k tokens "in" 3.2 s), the other reported an empty memory floor
      (Prometheus is not reachable from the head). Both causes are fixed; the leg just needs
      running again.
- [ ] **Score the review answers** for the five deployments that have them. The report does the
      scoring; only GLM has been read so far.

## Improvements noticed while measuring

- [ ] **Start steps run sequentially, which costs time on a two-server deployment.**
      `qwen3.8-27b-sglang` starts its head, waits for that server to answer, and only then starts
      dgx02 — the two are independent and have no reason to serialise. A `parallel = true` on the
      start list, or a step that backgrounds per node, would halve its boot.
- [ ] **`switch` only waits for the head's API.** With one server per node, dgx02 can fail to start
      and the switch still reports DONE. The wait belongs on every endpoint in `api_urls`, not just
      `api_url`.
- [ ] **Old runs carry no `--note`.** Everything before 00:44Z was recorded without conditions, so
      the report cannot show that some of it was taken during a 160 GiB download. Annotate those
      files once the pass is done.

## Prefetching now (started 22:31Z)

Images and weights are pulled ahead of the deployments that need them, so a first boot is not also
a download. Downloads happen **once**: the 27B weights are fetched on dgx01 and rsynced to dgx02
over CX7, and the Flash-Next checkpoint goes to the head only because `start.sh` rsyncs the worker
copy itself.

- [ ] `lmsysorg/sglang:qwen38-27b` and `vllm/vllm-openai:qwen38-flash-next` on both nodes
- [ ] Qwen3.8-27B NVFP4 (~24 GiB) + DSpark draft (~2.7 GiB) → dgx01, then rsync to dgx02
- [ ] Qwen3.8-Flash-Next NVFP4 (~133 GiB, 11 shards) → head

GLM measurements wait for these to finish: 160 GiB streaming through page cache on a UMA box is
not a background task, and decode numbers taken during it would have to be thrown away.

## Done

- [x] **EXL3 provisioned and proven** (2026-09-18). 201 GiB EXL3 + 189 GiB Engram staged; first boot
      took three attempts and produced three recipe fixes on the mirror: the NFS exporter cannot
      export its own container root on overlay2 (`07f307b`), `config.json` was optional in staging
      but required by the loader (`b56cddc`), and the exporter-reuse path hardlinked into a
      directory nothing exports (`72b1f3d`).
- [x] **Measured the batch profile instead of guessing** (`SPEC_METHOD=none`, 4 seqs, 128k). +15%
      peak decode, −25% single stream, and for long prompts concurrency buys ~7% — so the review
      lane keeps the shipped profile. Numbers in `deployments/deepseek-v4.1-flash.toml`.
- [x] **DS4 measured**: 72.5 tok/s peak at four streams, and long prompts *do* gain from concurrency
      here (+48%), unlike EXL3.
- [x] **Telemetry audited and turned off.** Both vLLM recipes posted a hardware fingerprint to
      `stats.vllm.ai` on every boot; EXL3 already opted out, GLM and DS4 now do too, HF telemetry
      with them. Qwen/SGLang has no default reporter. Verified from a live container, not inferred.
- [x] **Qwen3.8-27B added as the fourth deployment**, mirrored and pinned, provisioned on both nodes
      as two independent servers measured as one pool.
- [x] **Tools**: `llm-quickbench` (concurrency sweep, endpoint pools), `llm-longctx-probe` (prefill
      + memory floor from Prometheus, per-model tokenizer calibration), `dgx-model bench|longctx|sync`.
- [x] **Tools are tested** — 19 pytest cases over a stub OpenAI server, and `make check` is the gate.

## Standing rules learned the hard way

- **Check the memory floor after a long prefill, not after a boot.** A 3 GiB KV pool boots and
  passes a smoke test, then dies at ~470k of a 600k prompt.
- **The head runs its own copy of dgx-model.** New deployment files are invisible until
  `dgx-model sync`, and the only symptom is that the deployment does not appear.
- **A new deployment's first boot will fail on something.** Both EXL3 failures were recipe defects
  that only show on a kit other than the author's. Budget for it; fix them on the mirror.
- **Prompt sizes are not comparable across tokenizers.** The probe calibrates per model now; any
  older number without that calibration is a different prompt.
