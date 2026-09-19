"""The reasoning switch is measured, not remembered.

These pin the finding of 2026-09-19: chat templates spell the switch differently — GLM reads
`thinking`, Qwen reads `enable_thinking` — and a template ignores a variable it does not define.
Sending one spelling to a fleet that uses both measures thinking ON under a row labelled off, and
nothing in the response says so. Every case below is a server that was met here.
"""
from conftest import make_server, url


def test_the_glm_spelling_is_honoured_and_reported(thinking_probe):
    """A template that defines `thinking`: the narrow switch wins and the TOML line is ready."""
    srv = make_server(reasons=True, honours="thinking")
    try:
        r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    assert r["recommended"] == "thinking"
    assert r["candidates"]["thinking"]["verdict"] == "honoured"
    assert r["candidates"]["enable_thinking"]["verdict"] == "ignored"
    assert r["lane_extra_toml"] == "extra = { chat_template_kwargs = { thinking = false } }"


def test_the_qwen_spelling_is_found_even_though_the_other_one_is_accepted(thinking_probe):
    """The bug this tool exists for: `thinking` is accepted, ignored, and looks like a refusal."""
    srv = make_server(reasons=True, honours="enable_thinking")
    try:
        r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    assert r["candidates"]["thinking"]["verdict"] == "ignored", "a silently ignored kwarg is not a rejection"
    assert r["recommended"] == "enable_thinking"
    assert r["lane_extra_toml"] == "extra = { chat_template_kwargs = { enable_thinking = false } }"


def test_a_trace_reported_only_as_usage_tokens_still_counts(thinking_probe):
    """Flash-Next: `reasoning_content` is empty and the 198 reasoning tokens are in usage.

    Counting characters alone called this deployment one that never reasons, and then called every
    switch pointless — while it was spending most of a review budget on a trace the lane cannot
    read.
    """
    srv = make_server(reasons=True, honours="enable_thinking", reasoning_in_usage_only=True)
    try:
        r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    assert r["baseline"]["reasoning_chars"] == 0, "this server never fills reasoning_content"
    assert r["baseline"]["reasoning_tokens"] > 0, "and says so only in usage"
    assert r["candidates"]["thinking"]["verdict"] == "ignored"
    assert r["recommended"] == "enable_thinking"


def test_the_trace_is_found_under_either_delta_name(thinking_probe):
    """vLLM streams it as `reasoning`, SGLang as `reasoning_content`.

    Reading one name made a model that spends its whole budget thinking look like one that never
    thinks — and then called every switch pointless, which is the same mistake twice.
    """
    for key in ("reasoning", "reasoning_content"):
        srv = make_server(reasons=True, honours="thinking", reasoning_delta_key=key)
        try:
            r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30,
                                     log=lambda *a: None)
        finally:
            srv.shutdown()
        assert r["baseline"]["reasoning_chars"] > 0, key
        assert r["recommended"] == "thinking", key


def test_reasoning_effort_counts_when_no_template_variable_does(thinking_probe):
    srv = make_server(reasons=True, honours="reasoning_effort")
    try:
        r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    assert r["recommended"] == "reasoning_effort"
    assert r["lane_extra_toml"] == 'extra = { reasoning_effort = "none" }'


def test_a_model_that_never_reasons_asks_for_nothing(thinking_probe):
    """Both DeepSeek deployments: no trace to suppress, so `[lane].extra` stays empty."""
    srv = make_server(reasons=False)
    try:
        r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    assert r["recommended"] is None
    assert all(c["verdict"] == "no reasoning to suppress" for c in r["candidates"].values())
    assert r["lane_extra_toml"] == "extra = {}"


def test_a_server_that_refuses_the_object_is_not_confused_with_one_that_ignores_it(thinking_probe):
    srv = make_server(reasons=True, reject_thinking=True)
    try:
        r = thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    for name in ("thinking", "enable_thinking", "both"):
        assert r["candidates"][name]["verdict"] == "rejected"
        assert r["candidates"][name]["http_status"] == 400
    assert r["recommended"] is None
    assert r["lane_extra_toml"] == "extra = {}"


def test_the_probe_asks_the_way_a_lane_asks(thinking_probe):
    """Streamed, because a server can answer the two ways differently.

    glm-5.3-flash-himorishige returns a 6,000-character trace and an empty body when streamed, and
    a plain answer with no trace when not. A probe that asked the easy way said "nothing to switch
    off" about the deployment whose entire problem is where its findings go.
    """
    srv = make_server(reasons=True, honours="thinking")
    try:
        thinking_probe.probe(url(srv), "stub-model", max_tokens=8, timeout=30, log=lambda *a: None)
    finally:
        srv.shutdown()
    assert srv.requests, "no request reached the server"
    assert all(b.get("stream") for b in srv.requests), "every candidate must be asked the same way"


def test_the_prompt_is_the_workload_the_answer_is_for(thinking_probe):
    """A short puzzle is answered directly by a model that thinks hard about a real review.

    That is not hypothetical: with a two-line prompt, glm-5.3-flash-himorishige answered in the
    body with no trace, and the probe reported "nothing to switch off" about the deployment whose
    review answers are 6,000 characters of trace behind an empty body. The prompt is now one of
    the review cases, asked the way llm-quickbench asks it, at the review budget.
    """
    assert thinking_probe.PROMPT_CASE, "the probe should be using a real review case"
    assert "pull request" in thinking_probe.PROMPT
    assert len(thinking_probe.PROMPT) > 400


def test_the_probe_prompt_carries_no_answer_key(thinking_probe):
    """Same rule as the sweep: the model gets the language and the code, nothing else."""
    import json, pathlib
    cases = json.loads(pathlib.Path(thinking_probe.CASES_PATH).read_text())["cases"]
    case = next(c for c in cases if c["id"] == thinking_probe.PROMPT_CASE)
    low = thinking_probe.PROMPT.lower()
    assert case["expected"].lower() not in low
    assert case["defect"].lower() not in low
