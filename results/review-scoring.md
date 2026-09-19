# Review content: what each deployment actually found

Speed is measured everywhere else in this repo. This file is the other half: given the same five
planted defects (`bench/review_cases/cases.json`), which deployment finds them, and **where the
finding ends up** — in the message content a review lane reads, or only in a reasoning trace that
an OpenAI-compatible client discards.

Scored over every answer stored under `~/.local/state/dgx-model/bench/`, latest pass 2026-09-19.
Cases are rows because there are more deployments than cases and the fleet keeps growing; each
column is one deployment in one reasoning mode. A cell is that column's **best** result for the
case — **body** beats reas. — and `×n` is how many answers it has for that case, so `×1` is a
single observation and `×6` is six.

| case | DS4 | DS4.1 | BIZ off | himo | miaai off | miaai think | 27B | Next off | Next think |
|---|---|---|---|---|---|---|---|---|---|
| `php-discount-logic` — tier boundary | **miss** ×3 | **miss** ×3 | **body** ×3 | reas. ×2 | **body** ×4 | reas. ×1 | reas. ×6 | reas. ×4 | **miss** ×1 |
| `csharp-expiry-logic` — `Any` vs `All` | **body** ×3 | **body** ×3 | **body** ×3 | reas. ×2 | **body** ×4 | reas. ×1 | **body** ×6 | **body** ×4 | **body** ×1 |
| `csharp-cache-race` — unsafe `Dictionary` | **body** ×3 | **body** ×3 | **body** ×3 | reas. ×2 | **body** ×4 | reas. ×1 | reas. ×6 | reas. ×4 | reas. ×1 |
| `laravel-balance-race` — lost update | **body** ×3 | **body** ×3 | **body** ×3 | reas. ×2 | **body** ×4 | **body** ×1 | **body** ×6 | **body** ×4 | reas. ×1 |
| `laravel-missing-validation` — mass assignment | **body** ×3 | **body** ×3 | **body** ×3 | reas. ×2 | **body** ×4 | reas. ×1 | **body** ×6 | reas. ×4 | reas. ×1 |
| **found** | **4/5** | **4/5** | **5/5** | **5/5** | **5/5** | **5/5** | **5/5** | **5/5** | **4/5** |

`asked-off` columns (DS4, DS4.1, himo, 27B) are runs from before reasoning became a sweep axis: no
`thinking` value was sent, so they show the server's default rather than a request for either mode.
`BIZ off` is the pass running now; its think leg has not finished, so it has no think column yet.

## What this says

**Every deployment can find these defects. Not every deployment says so where a lane can read it.**
That is the whole result. `glm-5.3-flash-himorishige` scores 5/5 with an **empty message body in
every one of its ten answers** — a lane reading `choices[0].message.content` receives nothing from
the deployment that found the most. `glm-5.3-flash-miaai` with thinking off, and now
`glm-5.3-flash-bizuayeu`, are the deployments that put all five in the body.

**The discount case is the discriminating one.** Both DeepSeek deployments report the float return
type — a real secondary issue the case's `expected` also names — and never notice that `> 10000`
beside `>= 5000` gives an order of exactly ¥10,000 the 5% tier. They miss it in all three of their
answers, and they emit no reasoning at all, so there is no trace for it to hide in. Every GLM and
Qwen deployment flags it, usually as "the mixed `>` and `>=` is a classic off-by-one smell", and
usually inside the reasoning — except the two GLM kits running with thinking off, which say it in
the body.

**The empty bodies are a token-budget artifact as much as a model trait.** At the 1600-token review
budget, 23 of `qwen3.8-27b-sglang`'s 30 answers and all 10 of himorishige's ended with
`finish_reason=length`, the whole budget spent in the reasoning trace:

| deployment / mode | answers | empty body | of those, reasoning had content | finish=length |
|---|---|---|---|---|
| `deepseek-v4-flash` | 15 | 0 | 0 | 0 |
| `deepseek-v4.1-flash` | 15 | 0 | 0 | 0 |
| `glm-5.3-flash-bizuayeu` nothink | 15 | 0 | 0 | 15 |
| `glm-5.3-flash-himorishige` | 10 | 10 | 5 | 5 |
| `glm-5.3-flash-miaai` nothink | 20 | 0 | 0 | 0 |
| `glm-5.3-flash-miaai` think | 5 | 4 | 4 | 5 |
| `qwen3.8-27b-sglang` | 30 | 20 | 20 | 23 |
| `qwen3.8-flash-next` nothink | 20 | 12 | 12 | 14 |
| `qwen3.8-flash-next` think | 5 | 2 | 2 | 2 |

`qwen3.8-flash-next` is the fastest deployment measured here on every scenario, and in `nothink` —
the mode it was *asked* for and does not honour — 12 of its 20 review answers carry nothing in the
body. Throughput bought nothing the lane can use in those twelve. `glm-5.3-flash-bizuayeu` also
runs out of budget on every answer, but in the opposite place: it spends all 1600 tokens writing
the review, so the finding is there and the last paragraph is cut off.

## Method, and what it does not prove

The marks come from a keyword pass over the stored answers: the boundary case matches
`exactly 10000|off-by-one|boundary|>= 10000|10000 gets`, the expiry case `All(`, the cache and
wallet cases their concurrency vocabulary, the controller case `mass assignment|fillable|allow-list`.
Samples from every deployment were read to check the patterns against the prose, but a keyword pass
scores *mention*, not *diagnosis quality*: it cannot tell a finding that leads the review from one
buried in a list of five, and it would credit a model that named the right term for the wrong
reason. Sample counts are uneven — one think-mode answer against six for the 27B — so a `×1` cell
is an observation, not a rate.

What it is enough for: choosing what a review lane runs, and what `[lane].extra` should send. On
this evidence the choice is between a deployment that puts its findings in the body and one that
needs the lane to read reasoning too, and that is a property of the deployment, not of its tok/s.
