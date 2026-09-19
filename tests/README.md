# tests

Run them with `make test` (or `make check`, which also parses every deployment file and every
script under `bin/`).

**The gate: these must pass before a tool change is committed, and before a measurement taken with
a changed tool is trusted.** The reason is specific rather than ceremonial — the numbers these
tools produce decide what the pair serves, and three of the bugs found so far were silent:

- a fixed chars-per-token ratio made "the same" 115k prompt 325k tokens on another model, so two
  models were compared on different work;
- a shared prompt across requests is answered from the prefix cache, reporting throughput the
  server cannot sustain;
- an empty `worker` list fell through to the Compose branch and took down every command that calls
  `status()` — including `switch`, mid-operation.

None of those announce themselves in the output. That is what the suite is for.

The scripts under `bin/` have no `.py` suffix, because that is the interface they are used through;
`conftest.py` loads them by path with an explicit `SourceFileLoader`. The OpenAI-compatible stub
server there speaks enough of the protocol (streaming SSE, usage accounting, a 400 for an unknown
`chat_template_kwargs`) to exercise the paths that matter without a GPU.
