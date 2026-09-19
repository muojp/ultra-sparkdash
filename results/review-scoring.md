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
and the finding was in the trace. Latest pass 2026-09-19.

| deployment · mode | discount<br>boundary | mass<br>assignment | wallet<br>lost update | cache<br>race | expiry<br>Any/All | found |
|---|---|---|---|---|---|---|
| `deepseek-v4-flash` | partial 1/2 | **found** | **found** | **found** | **found** | **4/5** +1p |
| `deepseek-v4.1-flash` | partial 1/2 | **found** | **found** | **found** | **found** | **4/5** +1p |
| `glm-5.3-flash-bizuayeu · nothink` | partial 1/2 | **found** | **found** | **found** | **found** | **4/5** +1p |
| `glm-5.3-flash-bizuayeu · think` | partial 1/2 *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **4/5** +1p |
| `glm-5.3-flash-himorishige` | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **5/5** |
| `glm-5.3-flash-miaai · nothink` | **found** | **found** | **found** | **found** | **found** | **5/5** |
| `glm-5.3-flash-miaai · think` | partial 1/2 *(reas.)* | **found** *(reas.)* | **found** | **found** *(reas.)* | **found** *(reas.)* | **4/5** +1p |
| `qwen3.8-27b-sglang` | **found** *(reas.)* | **found** *(reas.)* | **found** | **found** *(reas.)* | **found** | **5/5** |
| `qwen3.8-27b-sglang (pool of 2)` | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **5/5** |
| `qwen3.8-flash-next · nothink` | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** *(reas.)* | **found** | **5/5** |
| `qwen3.8-flash-next · think` | **missed** | **found** *(reas.)* | partial 1/2 | **found** *(reas.)* | **found** | **3/5** +1p |

`partial n/2` is the symptom without the cause: the case's second signal hit and the first did not.
`+1p` in the last column counts those; they are not folded into the score, because a deployment
that never named the defect is not a deployment that found it. Rows without a mode suffix are runs
from before reasoning became a sweep axis — no `thinking` value was sent, so they show the server's
default rather than a request for either mode.

## What this says

**Every deployment finds most of these defects. Where it says so is what differs, and that is what
a lane feels.** `glm-5.3-flash-himorishige` scores 5/5 with an **empty message body in all ten of
its answers**: a lane reading `choices[0].message.content` receives nothing from the deployment
that found the most. `glm-5.3-flash-miaai` with thinking off is the only row that is 5/5 **and**
puts every finding in the body.

**Asking for thinking off moves findings into the body, where the server honours it.** The two GLM
kits show it from both sides in the same table: `glm-5.3-flash-bizuayeu · nothink` answers with a
full body and no trace, and the same deployment's `think` row has four of five answers with an
empty body and the findings in the trace. That contrast is what `[lane].extra` is set from.

**But every `nothink` row for a Qwen deployment above is mislabelled.** Templates spell the switch
differently: GLM reads `thinking`, Qwen reads `enable_thinking`, and a template silently ignores a
variable it does not define. The sweep sent only the GLM spelling, so `qwen3.8-27b-sglang` and
`qwen3.8-flash-next` were measured with thinking ON while the rows say off. Probed with `dgx-model thinking` on the
27B on 2026-09-19 while it was serving: `enable_thinking` false returns an empty trace and the same
answer, `thinking` false returns 729 characters of trace, and `reasoning_effort: "none"` also
works. `llm-quickbench` now sends both spellings in one object, and the Qwen rows here should be
re-measured before they are compared as off.

**The discount case is the discriminating one, and it is where partials cluster.** The planted
defect is that `> 10000` beside `>= 5000` gives an order of exactly ¥10,000 the 5% tier. Both
DeepSeek deployments, and both bizuayeu rows, report the float return type instead — a real
secondary issue the case's `expected` also names — and score partial: they reach for the tier
vocabulary without ever making the boundary claim. himorishige, the 27B and Flash-Next's nothink
row state it outright, usually as "the mixed `>` and `>=` is a classic off-by-one smell", and
usually inside the reasoning.

**A signal a model can satisfy by pasting the code is not a signal.** The boundary case used to
match on a bare `>=`, which every answer that showed a corrected `DiscountService` repeated, so
four deployments scored a case they had never mentioned. Tightening it to the claim itself is what
turned those into partials, and `tests/test_report.py` now keeps a quote-only answer at zero.

**The empty bodies are a token-budget artifact as much as a model trait.** At the 1600-token review
budget:

| deployment · mode | answers | empty body | of those, reasoning had content | finish=length |
|---|---|---|---|---|
| `deepseek-v4-flash` | 15 | 0 | 0 | 0 |
| `deepseek-v4.1-flash` | 15 | 0 | 0 | 0 |
| `glm-5.3-flash-bizuayeu · nothink` | 20 | 0 | 0 | 20 |
| `glm-5.3-flash-bizuayeu · think` | 5 | 4 | 4 | 5 |
| `glm-5.3-flash-himorishige` | 10 | 10 | 5 | 5 |
| `glm-5.3-flash-miaai · nothink` | 20 | 0 | 0 | 0 |
| `glm-5.3-flash-miaai · think` | 5 | 4 | 4 | 5 |
| `qwen3.8-27b-sglang` | 30 | 20 | 20 | 23 |
| `qwen3.8-flash-next · nothink` | 20 | 12 | 12 | 14 |
| `qwen3.8-flash-next · think` | 5 | 2 | 2 | 2 |

`qwen3.8-flash-next` is the fastest deployment measured here on every scenario, and in `nothink` —
the mode it was asked for and does not honour — 12 of its 20 review answers carry nothing in the
body. Throughput bought nothing the lane can use in those twelve. `glm-5.3-flash-bizuayeu` also
ends every nothink answer on `length`, but in the opposite place: it spends the whole budget
writing the review in the body, thinking out loud as it goes, and the last paragraph is cut off.

## Method, and what it does not prove

Scoring is `score_answer` in `bin/llm-bench-report`: each case carries two loose signals, the first
naming the defect and the second its vocabulary, and a partial hit is reported rather than rounded
to either end. Answers from every deployment were read against the prose to check the patterns. It
still scores *mention*, not *diagnosis quality*: it cannot tell a finding that leads a review from
one buried sixth in a list, and it credits the right term used for the wrong reason. Sample counts
are uneven — one think-mode answer against six for the 27B — so a single row is an observation, not
a rate. The HTML report renders the same scoring per deployment and case; this file is where the
conclusions live.

What it is enough for: choosing what a review lane runs, and what `[lane].extra` sends it. On this
evidence that choice is between a deployment that puts its findings in the body and one that needs
the lane to read the trace as well — a property of the deployment and its reasoning mode, not of
its tok/s.
