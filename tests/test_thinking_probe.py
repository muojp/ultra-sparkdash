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


def test_the_prompt_gives_the_model_something_to_think_about(thinking_probe):
    """A trivial prompt would answer without a trace and call every switch honoured."""
    assert "EVERY" in thinking_probe.PROMPT and "Any(" in thinking_probe.PROMPT
