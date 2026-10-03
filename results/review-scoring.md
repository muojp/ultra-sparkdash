# Review content: what each deployment actually found

Speed is measured everywhere else in this repo. This file is the other half: given the same five
planted defects (`bench/review_cases/cases.json`), which deployment finds them, and **where the
finding ends up** — in the message content a review lane reads, or only in a reasoning trace that
an OpenAI-compatible client discards.

Deployments read down and cases across, so the table grows with the fleet rather than sideways. A
row is one deployment in one reasoning mode, because a model asked for thinking on and the same
model asked for thinking off are two systems for a lane. Scored over every answer stored under
`~/.local/state/dgx-model/bench/` by the same `score_answer` the HTML report uses — one scorer, two
views — keeping each deployment's best attempt per case. `(reas.)` means the message body was empty
and the finding was in the trace. Last updated 2026-09-20.

| deployment · mode | discount<br>boundary | mass<br>assignment | wallet<br>lost update | cache<br>race | expiry<br>Any/All | found |
|---|---|---|---|---|---|---|
| `deepseek-v4-flash` | partial 1/2 | **found** | **found** | **found** | **found** | **4/5** +1p |
| `deepseek-v4.1-flash` | partial 1/2 | **found** | **found** | **found** | **found** | **4/5** +1p |
| `glm-5.3-flash-bizuayeu · nothink?` | partial 1/2 | **found** | **found** | **found** | **found** | **4/5** +1p |
| `glm-5.3-flash-bizuayeu · think` | partial 1/2 *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **4/5** +1p |
| `glm-5.3-flash-himorishige` | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **5/5** |
| `glm-5.3-flash-miaai · nothink?` | **found** | **found** | **found** | **found** | **found** | **5/5** |
| `glm-5.3-flash-miaai · think` | partial 1/2 *(reas.)* | **found** *(reas.)* | **found** | **found** *(reas.)* | **found** *(reas.)* | **4/5** +1p |
| `qwen3.8-27b-sglang` | **found** *(reas.)* | **found** *(reas.)* | **found** | **found** *(reas.)* | **found** | **5/5** |
| `qwen3.8-27b-sglang (pool of 2)` | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **5/5** |
| `qwen3.8-27b-sglang (pool of 2) · nothink?` | **missed** | **found** | **found** | **found** | **found** | **4/5** |
| `qwen3.8-27b-sglang (pool of 2) · think` | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **5/5** |
| `qwen3.8-flash-next · nothink` | **missed** | **found** | **found** | **found** | **found** | **4/5** |
| `qwen3.8-flash-next · nothink?` | **found** *(reas.)* | **found** *(reas.)* | **found** | **found** | **found** | **5/5** |
| `qwen3.8-flash-next · think` | **missed** | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **4/5** |
| `qwen3.8-flash-next-single · nothink` | **missed** | **found** | **found** | **found** | **found** | **4/5** |
| `qwen3.8-flash-next-single · think` | partial 1/2 *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **4/5** +1p |

`partial n/2` is the symptom without the cause: the case's second signal hit and the first did not.
`+1p` counts those; they are not folded into the score, because a deployment that never named the
defect is not one that found it.

**`nothink?` means the run cannot say what it sent.** Chat templates spell the switch differently —
GLM reads `thinking`, Qwen reads `enable_thinking` — and a template ignores a name it does not
define. Rows taken before `llm-quickbench` recorded its kwargs may therefore have been thinking ON
under an off label, and both Qwen deployments were exactly that until they were re-measured. A plain
`nothink` row sent a spelling the deployment's own probe says it honours.

## What this says

**Every deployment finds most of these defects. Where it says so is what differs, and that is what
a lane feels.** `glm-5.3-flash-himorishige` scores 5/5 with an **empty message body in all ten of
its answers**, and `dgx-model thinking` says nothing can change that: all four ways of asking for
thinking off are accepted and ignored, every one returning an empty body and 3.9–5.3k characters of
trace. It is the deployment that finds the most and the one a content-reading lane gets the least
from. `glm-5.3-flash-miaai` with thinking off is the only row that is 5/5 **and** in the body.

**Thinking off is worth a case, and worth a lot of time.** Both Qwen deployments trade the
tier-boundary case for a readable answer: with thinking on they find it, in the trace, and with
thinking off they lose it and put the other four in the body. What that buys is not small —
`qwen3.8-flash-next` answers one review in 4.3 s instead of 32 s, and 54 tok/s becomes 181 at eight
streams, because the 1,600-token budget stops going into a trace nobody reads.

**The discount case is the discriminating one, and where partials cluster.** The planted defect is
that `> 10000` beside `>= 5000` gives an order of exactly ¥10,000 the 5% tier. Both DeepSeek
deployments and both bizuayeu rows report the float return type instead — a real secondary issue
the case's `expected` also names — and score partial: they reach for the tier vocabulary without
making the boundary claim.

**A signal a model can satisfy by pasting the code is not a signal.** The boundary case used to
match on a bare `>=`, which every answer that showed a corrected `DiscountService` repeated, so
four deployments scored a case they had never mentioned. Tightening it to the claim itself turned
those into partials, and `tests/test_report.py` keeps a quote-only answer at zero.

**The empty bodies are a token-budget artifact as much as a model trait.** At the 1600-token review
budget:

| deployment · mode | answers | empty body | of those, reasoning had content | finish=length |
|---|---|---|---|---|
| `deepseek-v4-flash` | 15 | 0 | 0 | 0 |
| `deepseek-v4.1-flash` | 15 | 0 | 0 | 0 |
| `glm-5.3-flash-bizuayeu · nothink?` | 20 | 0 | 0 | 20 |
| `glm-5.3-flash-bizuayeu · think` | 5 | 4 | 4 | 5 |
| `glm-5.3-flash-himorishige` | 10 | 10 | 5 | 5 |
| `glm-5.3-flash-miaai · nothink?` | 20 | 0 | 0 | 0 |
| `glm-5.3-flash-miaai · think` | 5 | 4 | 4 | 5 |
| `qwen3.8-27b-sglang` | 30 | 20 | 20 | 23 |
| `qwen3.8-27b-sglang · nothink?` | 5 | 0 | 0 | 0 |
| `qwen3.8-27b-sglang · think` | 5 | 4 | 4 | 4 |
| `qwen3.8-flash-next · nothink` | 5 | 0 | 0 | 0 |
| `qwen3.8-flash-next · nothink?` | 25 | 12 | 12 | 14 |
| `qwen3.8-flash-next · think` | 15 | 9 | 9 | 9 |

`glm-5.3-flash-bizuayeu` ends every answer on `length` too, but in the opposite place: it spends
the whole budget writing the review in the body, thinking out loud as it goes, and the last
paragraph is cut off.

## Method, and what it does not prove

Scoring is `score_answer` in `bin/llm-bench-report`: each case carries two loose signals, the first
naming the defect and the second its vocabulary, and a partial hit is reported rather than rounded
to either end. Answers from every deployment were read against the prose to check the patterns. It
still scores *mention*, not *diagnosis quality*: it cannot tell a finding that leads a review from
one buried sixth in a list, and it credits the right term used for the wrong reason. Sample counts
are uneven — five answers for a re-measured mode against thirty for an old one — so a single row is
an observation, not a rate.

What it is enough for: choosing what a review lane runs, and what `[lane].extra` sends it. Each
deployment's value there comes from `dgx-model thinking` against that deployment while it was
serving, not from its model family: of the three GLM kits one honours `thinking`, one ignores every
spelling, and the fastest deployment in the fleet reports its trace only as usage tokens.

## Single-Spark addition (2026-09-20)

`qwen3.8-flash-next-single` was measured on dgx01 only, stock Mia-AiLab NVFP4, TP=1,
MTP=3, max_num_seqs=4, native 262k, FP8 KV. Thinking-off uses the probed
`enable_thinking=false` spelling; all 15 review answers have a body and finish normally.
Thinking-on has 12/15 empty bodies and 13/15 length-limited answers at the same 1600-token budget.
The table shows the scorer's chosen best attempt per case; it does not mean every on-mode
answer is trace-only (two wallet answers and one expiry answer had bodies).

The threshold case remains missed with thinking off and partial with thinking on. Manual
inspection also found a false positive about a missing Controller import in an off-mode
Laravel answer (Controller is already in that namespace), and an off-mode C# response that
claims a namespace typo then retracts it. The signal score does not penalize these extras.
Raw answers and the full conditions are in `results/qwen3.8-flash-next-single/`.
