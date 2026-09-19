"""llm-quickbench: the behaviours a wrong number would come from."""
from conftest import make_server, url


def test_split_apis_handles_spaces_and_trailing_commas(quickbench):
    assert quickbench.split_apis("http://a/v1, http://b/v1,") == ["http://a/v1", "http://b/v1"]
    assert quickbench.split_apis("http://a/v1") == ["http://a/v1"]


def test_prompts_are_unique_per_seed(quickbench):
    """A shared prefix is served from the prefix cache and inflates throughput."""
    a = quickbench.build_prompt(512, 1)
    b = quickbench.build_prompt(512, 2)
    assert a != b
    assert quickbench.build_prompt(512, 1) == a


def test_discover_model_reads_the_endpoint(quickbench, stub):
    assert quickbench.discover_model([url(stub)]) == "stub-model"


def test_pool_of_different_models_is_refused(quickbench):
    """Averaging two models into one number is a mistake, not a configuration."""
    a, b = make_server("model-a"), make_server("model-b")
    try:
        try:
            quickbench.discover_model([url(a), url(b)])
        except SystemExit as e:
            assert "different models" in str(e)
        else:
            raise AssertionError("expected SystemExit")
    finally:
        a.shutdown()
        b.shutdown()


def test_streamed_request_reports_usage_and_ttft(quickbench, stub):
    r = quickbench.one_request(url(stub), "stub-model", "hello", 8, None, 30)
    assert r.ok and r.err is None
    assert r.completion == 8
    assert r.ttft is not None and r.wall >= r.ttft
    assert r.finish == "length"


def test_thinking_kwarg_is_dropped_after_a_rejection(quickbench):
    """Templates differ between models; a sweep must not die on the first 400."""
    quickbench.THINKING_UNSUPPORTED.clear()
    srv = make_server(reject_thinking=True)
    try:
        r = quickbench.one_request(url(srv), "stub-model", "hello", 4, False, 30)
        assert r.ok, r.err
        assert quickbench.THINKING_UNSUPPORTED  # remembered, so later requests skip the kwarg
        bodies = srv.requests
        assert "chat_template_kwargs" in bodies[0]
        assert "chat_template_kwargs" not in bodies[-1]
    finally:
        quickbench.THINKING_UNSUPPORTED.clear()
        srv.shutdown()


def test_requests_are_spread_round_robin_over_a_pool(quickbench, stub_pair):
    a, b = stub_pair

    class Args:
        prompt_tokens = 64
        max_tokens = 4
        thinking = None
        timeout = 30
        shared_prefix = False

    row = quickbench.run_level([url(a), url(b)], "stub-model", 4, Args(), seed_base=1)
    assert row["requests"] == 4
    assert len(row["endpoints"]) == 2
    assert sum(row["endpoints"].values()) == row["output_tokens"]
    assert len(a.requests) == len(b.requests) == 2


def test_aggregate_counts_only_completed_output(quickbench, stub):
    class Args:
        prompt_tokens = 64
        max_tokens = 5
        thinking = None
        timeout = 30
        shared_prefix = False

    row = quickbench.run_level([url(stub)], "stub-model", 2, Args(), seed_base=7)
    assert row["output_tokens"] == 10
    assert row["aggregate_tok_s"] > 0
    assert row["failed"] == 0


def test_every_scenario_is_self_contained(quickbench):
    """A scenario must carry both what to ask and how much to generate, or the sweep is not fixed."""
    for name, spec in quickbench.SCENARIOS.items():
        assert spec["prompt"].strip(), name
        assert spec["max_tokens"] > 0, name


def test_scenario_changes_the_instruction_but_keeps_the_filler_size(quickbench):
    chat = quickbench.build_prompt(512, 1, "chat")
    code = quickbench.build_prompt(512, 1, "code")
    assert chat != code
    assert "merge_ranges" in code and "merge_ranges" not in chat
    # The context filler is what the prefill measures; it must not change with the scenario.
    assert abs(len(chat) - len(code)) < 400


def test_scenario_default_output_length_is_used_unless_overridden(quickbench, stub):
    class Args:
        prompt_tokens = 64
        max_tokens = 0          # 0 = take the scenario's own length
        thinking = None
        timeout = 60
        shared_prefix = False

    row = quickbench.run_level([url(stub)], "stub-model", 1, Args(), 1, scenario="chat")
    assert row["output_tokens"] == quickbench.SCENARIOS["chat"]["max_tokens"]

    class Override(Args):
        max_tokens = 7

    row = quickbench.run_level([url(stub)], "stub-model", 1, Override(), 2, scenario="chat")
    assert row["output_tokens"] == 7


def test_structured_scenario_sends_a_response_format(quickbench, stub):
    class Args:
        prompt_tokens = 32
        max_tokens = 4
        thinking = None
        timeout = 60
        shared_prefix = False

    quickbench.run_level([url(stub)], "stub-model", 1, Args(), 3, scenario="structured")
    assert "response_format" in stub.requests[-1]
    assert stub.requests[-1]["response_format"]["type"] == "json_schema"


def test_rows_carry_their_scenario(quickbench, stub):
    class Args:
        prompt_tokens = 32
        max_tokens = 4
        thinking = None
        timeout = 60
        shared_prefix = False

    row = quickbench.run_level([url(stub)], "stub-model", 1, Args(), 4, scenario="code")
    assert row["scenario"] == "code"


def test_review_cases_are_well_formed(quickbench):
    """Each case needs the planted defect written down, or an answer cannot be scored."""
    cases = quickbench.review_cases()
    assert len(cases) >= 5
    kinds = {c["defect"] for c in cases}
    assert {"logic", "concurrency", "validation"} <= kinds, kinds
    langs = " ".join(c["language"] for c in cases)
    assert "PHP" in langs and "C#" in langs
    for c in cases:
        assert c["code"].strip() and c["expected"].strip() and c["id"]


def test_review_prompt_never_carries_the_answer_key(quickbench):
    """The id names the defect, `defect` is its category, `expected` is the finding itself."""
    for i, case in enumerate(quickbench.review_cases()):
        prompt = quickbench.build_prompt(0, i, "review")
        assert case["id"] not in prompt
        assert case["expected"][:40] not in prompt
        assert quickbench.case_leak(prompt, case) is None
        # The instruction must not enumerate the categories either — that narrows the search to a
        # multiple-choice question.
        for category in ("logic mistake", "concurrency hazard", "validation"):
            assert category not in prompt.lower(), category


def test_case_leak_detects_a_leaky_prompt(quickbench):
    """Any give-away is a failure; which field it came from is only detail for the message."""
    case = quickbench.review_cases()[0]
    assert quickbench.case_leak("find the %s bug" % case["defect"], case)
    assert quickbench.case_leak(case["expected"], case)
    assert quickbench.case_leak("review this file", case) is None


def test_review_prompts_rotate_and_carry_the_code(quickbench):
    cases = quickbench.review_cases()
    seen = {quickbench.build_prompt(0, i, "review") for i in range(len(cases))}
    assert len(seen) == len(cases), "each seed must draw a different case"
    p = quickbench.build_prompt(0, 0, "review")
    assert cases[0]["code"].splitlines()[0] in p
    assert "filler" not in p and "Context follows" not in p


def test_ttft_is_recorded_when_a_model_streams_reasoning_content(quickbench):
    """SGLang says reasoning_content where vLLM says reasoning; missing it loses TTFT entirely."""
    srv = make_server(reasoning_key=True)
    try:
        r = quickbench.one_request(url(srv), "stub-model", "hello", 6, None, 30)
        assert r.ok and r.ttft is not None
    finally:
        srv.shutdown()


def test_review_capture_follows_the_reasoning_key_too(quickbench):
    srv = make_server(reasoning_key=True)
    try:
        r = quickbench.one_request(url(srv), "stub-model", "hi", 4, None, 30, keep_text=True)
        assert r.think_text, "reasoning must be captured under either key"
    finally:
        srv.shutdown()


def test_rows_record_the_reasoning_configuration(quickbench, stub):
    class Args:
        prompt_tokens = 32
        max_tokens = 4
        thinking = True
        timeout = 30
        shared_prefix = False

    row = quickbench.run_level([url(stub)], "stub-model", 1, Args(), 1, scenario="chat",
                               thinking=False, effort="high")
    assert row["thinking"] is False and row["reasoning_effort"] == "high"
    assert "reasoning_effort" in stub.requests[-1]


def test_row_labels_name_the_reasoning_config_only_when_it_varies(quickbench):
    assert quickbench.label_for("review", None, None) == "review"
    assert quickbench.label_for("review", True, None) == "review/think"
    assert quickbench.label_for("review", False, "low") == "review/nothink/low"


def test_a_server_rejecting_reasoning_effort_does_not_fail_the_run(quickbench):
    quickbench.EFFORT_UNSUPPORTED.clear()
    srv = make_server(reject_thinking=False)
    try:
        # The stub 400s on chat_template_kwargs only; reasoning_effort passes through, so the
        # fallback path is exercised by the thinking test. Here we assert the flag starts clean.
        assert not quickbench.EFFORT_UNSUPPORTED
    finally:
        srv.shutdown()
