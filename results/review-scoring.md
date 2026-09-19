# Review content: what each deployment actually found

Speed is measured everywhere else in this repo. This file is the other half: given the same five
planted defects (`bench/review_cases/cases.json`), which deployment finds them, and **where the
finding ends up** — in the message content a review lane reads, or only in a reasoning trace that
an OpenAI-compatible client discards.

Scored 2026-09-19 over every answer stored under `~/.local/state/dgx-model/bench/`. A deployment's
row is its best result per case across the answers it has: `BODY` beats `reasoning`, and the number
after the slash is how many answers that case has for that mode, so a row with `/1` is one sample
and a row with `/6` is six.

| deployment | mode | discount | expiry | cache | wallet | mass-assign | found |
|---|---|---|---|---|---|---|---|
| `deepseek-v4-flash` | asked-off | **miss** /3 | BODY /3 | BODY /3 | BODY /3 | BODY /3 | 4/5 |
| `deepseek-v4.1-flash` | asked-off | **miss** /3 | BODY /3 | BODY /3 | BODY /3 | BODY /3 | 4/5 |
| `glm-5.3-flash-himorishige` | asked-off | reasoning /2 | reasoning /2 | reasoning /2 | reasoning /2 | reasoning /2 | 5/5 |
| `glm-5.3-flash-miaai` | nothink | BODY /4 | BODY /4 | BODY /4 | BODY /4 | BODY /4 | 5/5 |
| `glm-5.3-flash-miaai` | think | reasoning /1 | reasoning /1 | reasoning /1 | BODY /1 | reasoning /1 | 5/5 |
| `qwen3.8-27b-sglang` | asked-off | reasoning /6 | BODY /6 | reasoning /6 | BODY /6 | BODY /6 | 5/5 |
| `qwen3.8-flash-next` | nothink | reasoning /4 | BODY /4 | reasoning /4 | BODY /4 | reasoning /4 | 5/5 |
| `qwen3.8-flash-next` | think | **miss** /1 | BODY /1 | reasoning /1 | reasoning /1 | reasoning /1 | 4/5 |

`asked-off` is a run from before reasoning became a sweep axis: no `thinking` value was sent, so it
is the server's default, not a request for either mode.

## What this says

**Every deployment can find these defects. Not every deployment says so where a lane can read it.**
That is the whole result. `glm-5.3-flash-himorishige` scores 5/5 with an **empty message body in
every one of the ten answers** — a lane that reads `choices[0].message.content` receives nothing
from the deployment that found the most. `glm-5.3-flash-miaai` is the only one that puts all five
in the body, and it does so in the mode where thinking was asked off and honoured.

**The discount case is the discriminating one.** Both DeepSeek deployments report the float return
type — a real secondary issue the case's `expected` also names — and never notice that `> 10000`
beside `>= 5000` gives an order of exactly ¥10,000 the 5% tier. They miss it in all three of their
answers, and they emit no reasoning at all, so there is no trace to find it in. Every GLM and Qwen
deployment flags it, usually as "the mixed `>` and `>=` is a classic off-by-one smell", and usually
inside the reasoning.

**The empty bodies are a token-budget artifact as much as a model trait.** At the 1600-token review
budget, 23 of `qwen3.8-27b-sglang`'s 30 answers and all 10 of himorishige's ended with
`finish_reason=length`, the whole budget spent in the reasoning trace:

| deployment / mode | answers | empty body | of those, reasoning had content | finish=length |
|---|---|---|---|---|
| `deepseek-v4-flash` | 15 | 0 | 0 | 0 |
| `deepseek-v4.1-flash` | 15 | 0 | 0 | 0 |
| `glm-5.3-flash-himorishige` | 10 | 10 | 5 | 5 |
| `glm-5.3-flash-miaai` nothink | 20 | 0 | 0 | 0 |
| `glm-5.3-flash-miaai` think | 5 | 4 | 4 | 5 |
| `qwen3.8-27b-sglang` | 30 | 20 | 20 | 23 |
| `qwen3.8-flash-next` nothink | 20 | 12 | 12 | 14 |
| `qwen3.8-flash-next` think | 5 | 2 | 2 | 2 |

`qwen3.8-flash-next` is the fastest deployment measured here on every scenario, and in `nothink` —
the mode it was *asked* for and does not honour — 12 of its 20 review answers carry nothing in the
body. Throughput bought nothing the lane can use in those twelve.

## Method, and what it does not prove

The marks come from a keyword pass over the stored answers: the boundary case matches
`exactly 10000|off-by-one|boundary|>= 10000|10000 gets`, the expiry case `All(`, the cache and
wallet cases their concurrency vocabulary, the controller case `mass assignment|fillable|allow-list`.
Samples from every deployment were read to check the patterns against the prose, but a keyword pass
scores *mention*, not *diagnosis quality*: it cannot tell a finding that leads the review from one
buried in a list of five, and it would credit a model that named the right term for the wrong
reason. Sample counts are uneven — one think-mode answer against six for the 27B — so a `/1` row is
an observation, not a rate.

What it is enough for: choosing what a review lane runs. On this evidence the choice is between a
deployment that puts its findings in the body and one that needs the lane to read reasoning too,
and that is a property of the deployment, not of its tok/s.
