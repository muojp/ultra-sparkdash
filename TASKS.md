# TASKS

Working state for the dgx pair, kept here rather than in a chat log or anyone's head. Same rule as
`deployments/README.md`: if a step is missing here, it is missing. Numbers live in the deployment
files; this file only says what is done, what is running, and what is next.

_Last updated: 2026-09-19 07:55Z_

## The gate

`make check` — pytest plus a parse of every deployment file and script — **passes before a tool
change is committed, and before a measurement taken with a changed tool is trusted.** See
`tests/README.md` for why: every bug it has caught so far was silent in the output.

## Now — seven of seven measured

| deployment | decode (5 scenarios) | long context | review content |
|---|---|---|---|
| `glm-5.3-flash-himorishige` | done | done | 5/5, every finding in the reasoning trace |
| `deepseek-v4-flash` | done | done | 4/5 + 1 partial, all in the body |
| `deepseek-v4.1-flash` | done | done | 4/5 + 1 partial, all in the body |
| `qwen3.8-27b-sglang` | done (pool and single node) | **redo** | 5/5, three of five only in the trace |
| `glm-5.3-flash-miaai` | done | done (2 of 4 long requests dropped) | 5/5 in the body — the only row that is both |
| `qwen3.8-flash-next` | done — fastest everywhere | done | 5/5, four of five only in the trace |
| `glm-5.3-flash-bizuayeu` | done | done | 4/5 + 1 partial, all in the body |

`results/review-scoring.md` is the content half of that last column, and the report renders the
same scoring per case.

**bizuayeu, measured 2026-09-19 06:43–07:54Z.** 22.3 chat, 25.8 code, 21.7 essay, 25.8 review,
11.5 structured tok/s peak — and **no concurrency gain anywhere**, because the kit's profile pins
`context.max_num_seqs = 1`: every column above c=1 is a queue in front of one sequence, which the
long-context leg shows plainly (four streams finishing at 296, 590, 884, 1178 s, exactly serial).
That value is the kit's validated one and was left alone; compare this deployment with the other
six at c=1 and read the rest as this profile under load. Long context: 162,553 prompt tokens,
prefill 523 tok/s single and 552 at four streams, memory floor 4.00 GiB on the head and 7.16 on the
worker. Thinking off is honoured (`reason_p50` 1600 with it on, 0 with it off), which is what
`[lane].extra` for it is set from.

- [x] **bizuayeu unblocked and measured.** The launcher ignored `HF_HOME` in three places while the
      downloader honoured it; fixed on the mirror as one `cache_root()`, with three more derived
      commits: a profile-settable API bind address, a per-node image ID (the head is on the classic
      image store and the worker on the containerd snapshotter, so one image is two IDs), and the
      start steps' redirection moved onto the subshell so a detached rank does not hold the ssh
      session open. Traps are in `deployments/glm-5.3-flash-bizuayeu.toml`.
- [ ] **Re-run the Qwen 27B long-context leg.** Its two attempts are void: one replayed a cached
      prompt (fixed seeds — 140k tokens "in" 3.2 s), the other reported an empty memory floor
      (Prometheus is not reachable from the head). Both causes are fixed; the leg just needs
      running again. Probe the thinking kwarg on the same switch — `[lane].extra` for it is still
      empty because nothing has ever sent it one.
- [ ] **Probe `glm-5.3-flash-himorishige` with thinking off.** It finds all five defects and puts
      every one of them in a reasoning trace behind an empty body, and nobody has ever asked it for
      thinking off. Whether the kwarg moves those findings into the body decides whether the lane
      can use the deployment that scores highest.
- [x] **Score the review answers.** All seven, in `results/review-scoring.md`. Scoring the stored
      answers also exposed a bug in the cases: the discount case matched a bare `>=`, which every
      answer that pasted a corrected `DiscountService` repeated, so four deployments scored a
      boundary they had never mentioned.

## Improvements noticed while measuring

- [ ] **Start steps run sequentially, which costs time on a two-server deployment.**
      `qwen3.8-27b-sglang` starts its head, waits for that server to answer, and only then starts
      dgx02 — the two are independent and have no reason to serialise. A `parallel = true` on the
      start list, or a step that backgrounds per node, would halve its boot.
- [ ] **`switch` only waits for the head's API.** With one server per node, dgx02 can fail to start
      and the switch still reports DONE. The wait belongs on every endpoint in `api_urls`, not just
      `api_url`.
- [ ] **Long-context prompts are not the same size across deployments.** The probe calibrates
      chars-per-token per model, and the result still lands between 131k and 163k tokens for the
      same `--tokens 115000`. Prefill tok/s divides by the real count, so the rate is comparable;
      wall time is not, and neither is the memory floor a 163k prompt reaches against a 131k one.
- [ ] **Old runs carry no `--note`.** Everything before 00:44Z was recorded without conditions, so
      the report cannot show that some of it was taken during a 160 GiB download. Annotate those
      files once the pass is done.

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
