"""llm-bench-report: the page must keep accumulating without changing shape."""
import json
import pathlib


def write_run(state, kind, stamp, deployment, payload):
    d = state / kind
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{stamp}-{deployment}.json").write_text(json.dumps(payload))


def bench_payload(model, rows, note=""):
    return {"model": model, "note": note, "rows": rows}


def row(scenario, conc, agg, per_stream=10.0, ttft=1.0):
    return {"scenario": scenario, "concurrency": conc, "aggregate_tok_s": agg,
            "per_stream_tok_s": per_stream, "ttft_p50_s": ttft, "requests": conc,
            "output_tokens": conc * 100, "wall_s": 10.0}


def test_latest_run_wins_and_history_is_kept(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha",
              bench_payload("m", [row("chat", 1, 10.0)], note="under load"))
    write_run(state, "bench", "20260102T000000+0000", "alpha",
              bench_payload("m", [row("chat", 1, 20.0)]))
    runs = report.load_runs(state, "bench")
    assert len(runs) == 2
    latest = report.latest_matrix(runs)
    assert latest[("alpha", "chat", 1)]["aggregate_tok_s"] == 20.0

    out = report.render(runs, [], tmp_path / "r.html")
    page = out.read_text()
    assert "20.0" in page and "10.0" in page, "history must survive, not just the latest"
    assert "under load" in page, "a run's note must reach the page"


def test_two_deployments_appear_in_one_table(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha", bench_payload("m1", [row("code", 4, 40.0)]))
    write_run(state, "bench", "20260101T010000+0000", "beta", bench_payload("m2", [row("code", 4, 70.0)]))
    runs = report.load_runs(state, "bench")
    page = report.render(runs, [], tmp_path / "r.html").read_text()
    assert "alpha" in page and "beta" in page
    assert "best" in page, "the faster cell should be marked"


def test_longctx_floor_is_rendered(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "longctx", "20260101T000000+0000", "alpha", {
        "model": "m", "rows": [
            {"label": "single", "streams": 1, "prompt_tokens": 100000,
             "prefill_aggregate_tok_s": 900.0, "mem_floor_gib": {"dgx01": 3.1, "dgx02": 8.0}},
            {"label": "concurrent", "streams": 4, "prompt_tokens": 100000,
             "prefill_aggregate_tok_s": 950.0, "mem_floor_gib": {"dgx01": 2.5, "dgx02": 7.9}}]})
    runs = report.load_runs(state, "longctx")
    page = report.render([], runs, tmp_path / "r.html").read_text()
    assert "dgx01 2.50" in page and "950.0" in page


def test_a_corrupt_run_file_does_not_break_the_page(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha", bench_payload("m", [row("chat", 1, 10.0)]))
    (state / "bench" / "20260101T010000+0000-broken.json").write_text("{not json")
    runs = report.load_runs(state, "bench")
    assert len(runs) == 1
    assert report.render(runs, [], tmp_path / "r.html").exists()


def test_review_scoring_separates_found_from_missed(report):
    cases = report.load_cases()
    assert cases, "cases file must be readable by the report"
    race = cases["laravel-balance-race"]
    found = report.score_answer(race, "This is a race: two withdrawals read the same balance. "
                                      "Wrap it in a transaction with lockForUpdate().")
    missed = report.score_answer(race, "The method looks fine; maybe rename the variable.")
    assert found == (2, 2)
    assert missed == (0, 2)


def test_partial_hit_is_reported_as_partial(report):
    cases = report.load_cases()
    race = cases["csharp-cache-race"]
    partial = report.score_answer(race, "Two threads can race here.")   # names it, no fix named
    assert partial[0] == 1 and partial[1] == 2


def test_review_section_keeps_the_best_attempt(report, tmp_path):
    state = tmp_path / "state"
    ans_bad = {"case": "csharp-expiry-logic", "text": "looks fine"}
    ans_good = {"case": "csharp-expiry-logic", "text": "Any() should be All(); also DateTime.Now vs UtcNow"}
    write_run(state, "bench", "20260101T000000+0000", "alpha",
              {"model": "m", "rows": [{"scenario": "review", "concurrency": 1, "aggregate_tok_s": 5,
                                       "answers": [ans_bad]}]})
    write_run(state, "bench", "20260101T010000+0000", "alpha",
              {"model": "m", "rows": [{"scenario": "review", "concurrency": 1, "aggregate_tok_s": 5,
                                       "answers": [ans_good]}]})
    runs = report.load_runs(state, "bench")
    scored = report.review_section(runs, report.load_cases())
    hit, total, _, _, _ = scored[("alpha", "csharp-expiry-logic")]
    assert hit == total, "the better attempt must win"


def test_review_table_renders_even_with_no_answers(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha",
              {"model": "m", "rows": [{"scenario": "chat", "concurrency": 1, "aggregate_tok_s": 5}]})
    runs = report.load_runs(state, "bench")
    page = report.render(runs, [], tmp_path / "r.html").read_text()
    assert "Review cases" in page and "--scenario review" in page


def test_runs_recorded_under_an_old_name_join_the_new_one(report, tmp_path):
    """A rename must not split a deployment's history in two."""
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "glm-5.3-flash",
              bench_payload("m", [row("chat", 1, 10.0)]))
    write_run(state, "bench", "20260102T000000+0000", "glm-5.3-flash-himorishige",
              bench_payload("m", [row("chat", 2, 20.0)]))
    runs = report.load_runs(state, "bench")
    assert {r["_deployment"] for r in runs} == {"glm-5.3-flash-himorishige"}
    page = report.render(runs, [], tmp_path / "r.html").read_text()
    assert "recorded as glm-5.3-flash" in page, "the original name stays visible in the history"


def test_a_row_built_from_two_runs_shows_the_span(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha", bench_payload("m", [row("chat", 1, 10.0)]))
    write_run(state, "bench", "20260102T120000+0000", "alpha", bench_payload("m", [row("chat", 4, 30.0)]))
    runs = report.load_runs(state, "bench")
    page = report.render(runs, [], tmp_path / "r.html").read_text()
    assert "2026-01-01 00:00 … 2026-01-02 12:00" in page


def test_a_row_from_one_run_shows_a_single_time(report, tmp_path):
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha",
              bench_payload("m", [row("chat", 1, 10.0), row("chat", 4, 30.0)]))
    runs = report.load_runs(state, "bench")
    page = report.render(runs, [], tmp_path / "r.html").read_text()
    assert "…" not in page.split("<h2>Long context")[0]


def test_a_finding_in_the_reasoning_trace_still_counts(report, tmp_path):
    """Empty content with the finding in the trace is not the same result as missing it."""
    state = tmp_path / "state"
    write_run(state, "bench", "20260101T000000+0000", "alpha",
              {"model": "m", "rows": [{"scenario": "review", "concurrency": 1, "aggregate_tok_s": 5,
                                       "answers": [{"case": "csharp-expiry-logic", "text": "",
                                                    "reasoning": "Any() should be All() here; "
                                                                 "also DateTime.Now vs UtcNow"}]}]})
    runs = report.load_runs(state, "bench")
    scored = report.review_section(runs, report.load_cases())
    hit, total, _, _, source = scored[("alpha", "csharp-expiry-logic")]
    assert hit == total and source == "reasoning only"
    page = report.render(runs, [], tmp_path / "r.html").read_text()
    assert "in reasoning" in page
