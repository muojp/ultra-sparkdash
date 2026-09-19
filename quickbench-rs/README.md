# quickbench-rs

A Rust/tokio rewrite of `bin/llm-quickbench`, kept alongside it rather than replacing it.

## Why it exists, and what it measured

It was written to remove a client-side bottleneck that turned out not to exist. Measuring two
servers as a pool reached 141.6 tok/s at concurrency 8 against a single node's 104.3, and 1.36x
looked like the client saturating. It was the wrong comparison: the pool runs four streams per
node, so the like-for-like figure is a single node at concurrency **4** — 70.5 tok/s. Two of those
is 141.0 against a measured 141.6. The pool scales linearly; one node simply flattens past four
streams.

Run side by side afterwards, the two clients agree:

| chat, concurrency 8, pool of 2 | aggregate | per-stream | ttft p50 |
|---|---:|---:|---:|
| Python client | 141.6 | 20.6 | 1.64 s |
| Rust client | 137.8 | 20.4 | 1.63 s |

So this is not a performance fix. It is a better base for what comes next — one process can hold
many more concurrent streams than a thread per request, which matters if the pool ever grows past
two nodes or the sweep goes past eight streams.

## Building

There is no toolchain on the head, so build in a container on the machine that will run it:

```sh
rsync -a --exclude target quickbench-rs/ <head>:~/quickbench-rs/
ssh <head> 'cd ~/quickbench-rs && docker run --rm -v "$PWD":/src -w /src \
  -v "$HOME/.cargo-registry":/usr/local/cargo/registry rust:1-slim-bookworm \
  cargo build --release'
ssh <head> 'cp ~/quickbench-rs/target/release/llm-quickbench ~/dgx-model/bin/llm-quickbench-rs'
```

`cargo test` covers the parts that decide whether a row has numbers at all: both spellings of the
reasoning delta (`reasoning` for vLLM, `reasoning_content` for SGLang), an empty content delta not
counting as a first token, usage and finish-reason capture, the percentile and median arithmetic,
and that a review prompt never carries its case's answer key.

## Flags

Same surface as the Python client: `--api` (comma-separated endpoints form one pool), `--model`,
`-c`, `--scenario` (`all` or a comma list), `--max-tokens`, `--prompt-tokens`, `--no-thinking`,
`--shared-prefix`, `--warmup`, `--timeout`, `--json`, `--note`, `--cases`. The JSON it writes is the
same shape, so `bin/llm-bench-report` reads either.
