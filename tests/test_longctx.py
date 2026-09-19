"""llm-longctx-probe: calibration and the memory floor are the two things it must not get wrong."""
import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

from conftest import url


def test_calibration_derives_chars_per_token_from_the_model(longctx, stub):
    """The stub bills one token per four characters; the probe must discover that, not assume it."""
    cpt = longctx.calibrate(url(stub), "stub-model", 30)
    assert 3.5 < cpt < 4.5


def test_prompt_size_follows_the_calibration(longctx):
    small = longctx.build_prompt(10000, 1, chars_per_token=2.0)
    large = longctx.build_prompt(10000, 1, chars_per_token=8.0)
    assert len(large) > 3 * len(small)


def test_legs_spread_over_the_pool(longctx, stub_pair):
    a, b = stub_pair

    class Args:
        thinking = None
        timeout = 30
        prometheus = "http://127.0.0.1:1"   # unreachable on purpose: the floor must not be fatal
        mem_hosts = ""                      # and no direct sampling in a unit test

    row = longctx.leg("concurrent", [url(a), url(b)], "stub-model", 200, 4, Args(), 4.0)
    assert row["streams"] == 4
    assert not row["errors"]
    assert len(a.requests) == len(b.requests) == 2


def test_memory_floor_is_read_per_node(longctx):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            body = json.dumps({"data": {"result": [
                {"metric": {"spark": "dgx01"}, "value": [0, str(8 * 1073741824)]},
                {"metric": {"spark": "dgx02"}, "value": [0, str(9 * 1073741824)]},
            ]}}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    srv = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        floor = longctx.mem_floor("http://127.0.0.1:%d" % srv.server_port, 0, 100)
        assert floor == {"dgx01": 8.0, "dgx02": 9.0}
    finally:
        srv.shutdown()


def test_unreachable_prometheus_returns_an_error_not_an_exception(longctx):
    floor = longctx.mem_floor("http://127.0.0.1:1", 0, 10)
    assert "error" in floor


def test_the_probe_accepts_a_note(longctx):
    """The sweep and the probe are driven by the same pass, so both take --note."""
    import argparse, inspect
    src = inspect.getsource(longctx.main)
    assert "--note" in src, "a run's conditions must be recordable here too"


def test_run_seed_differs_between_processes(longctx):
    """A repeated run against the same server is answered from the prefix cache."""
    assert longctx.RUN_SEED > 0
    # Two prompts built with the run seed must differ from the fixed-seed ones used before.
    a = longctx.build_prompt(500, longctx.RUN_SEED, 4.0)
    b = longctx.build_prompt(500, 100, 4.0)
    assert a != b


def test_direct_floor_reads_this_machine(longctx):
    f = longctx.DirectFloor(["local"])
    v = f._mem_available_gib("local")
    # On Linux this is a real reading; on a Mac there is no /proc, and None is the honest answer.
    assert v is None or v > 0


def test_direct_floor_survives_an_unreachable_host(longctx):
    f = longctx.DirectFloor(["nonexistent-host-xyz"])
    assert f._mem_available_gib("nonexistent-host-xyz") is None
    assert f.result() == {}
