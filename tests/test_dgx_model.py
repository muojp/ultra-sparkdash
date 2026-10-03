"""dgx-model: the deployment files themselves, and the logic that reads them.

The container-detection test exists because a single-node deployment with `worker = []` once made
every command that calls status() raise KeyError('compose_project').
"""
import pytest


def test_every_deployment_file_has_what_the_tool_reads(dgx_model):
    deps = dgx_model.load_deployments()
    assert deps, "no deployments found"
    for name, d in deps.items():
        for key in ("name", "served_model", "api_url", "boot_timeout_s", "host"):
            assert key in d, f"{name} is missing {key}"
        assert d["name"] == name
        assert d["host"].get("recipe_dir"), f"{name} has no recipe_dir"
        for step in ("start", "stop"):
            assert d["host"].get(step), f"{name} has no {step} command"
            assert all(isinstance(c, list) for c in d["host"][step]), f"{name}: {step} must be argv lists"
        # Detection needs one of the two, and deployment_containers refuses the rest.
        assert d.get("compose_project") or d.get("containers"), f"{name} has neither compose_project nor containers"


def test_file_name_matches_the_deployment_name(dgx_model):
    """`dgx-model switch <name>` takes the name, people look for the file: a mismatch hides one."""
    import pathlib
    root = pathlib.Path(dgx_model.__file__).resolve().parent.parent / "deployments"
    for f in sorted(root.glob("*.toml")):
        import tomllib
        assert tomllib.load(f.open("rb"))["name"] == f.stem, f


def test_deployment_names_are_unique_per_model_family(dgx_model):
    """Three GLM-5.3 recipes coexist, so the model no longer identifies the deployment."""
    deps = dgx_model.load_deployments()
    glm = [n for n in deps if n.startswith("glm-5.3-flash")]
    assert len(glm) == len(set(glm))
    for n in glm:
        assert n != "glm-5.3-flash", "bare model name is ambiguous; qualify it with whose recipe it is"


def test_every_lane_states_its_reasoning_setting(dgx_model):
    """`[lane].extra` is what the review lane merges into the request body.

    An absent key and an empty table read the same downstream, so the file has to carry the key
    even when the answer is "send nothing": that is the difference between a deployment whose
    reasoning behaviour was measured and one nobody has probed yet. The measured ones are in
    results/review-scoring.md; two servers here ignore the kwarg entirely and one honours it.
    """
    for name, d in dgx_model.load_deployments().items():
        lane = d.get("lane") or {}
        assert "extra" in lane, f"{name}: [lane].extra must be stated, {{}} if nothing is sent"
        assert isinstance(lane["extra"], dict), f"{name}: [lane].extra must be a table"


def test_pool_deployments_declare_where_the_client_runs(dgx_model):
    """api_urls without bench_from would be measured from wherever the operator happens to be."""
    for name, d in dgx_model.load_deployments().items():
        if len(d.get("api_urls") or []) > 1:
            assert d.get("bench_from") in ("head", "local"), f"{name}: api_urls needs bench_from"


def test_empty_worker_list_means_nothing_runs_there(dgx_model, monkeypatch):
    calls = []
    monkeypatch.setattr(dgx_model, "sh", lambda *a, **k: calls.append(a) or _R())
    d = {"name": "single", "host": {"worker": "dgx02"}, "containers": {"head": ["x"], "worker": []}}
    assert dgx_model.deployment_containers(d, "worker") == []
    assert not calls, "an empty side must not shell out at all"


def test_named_containers_are_filtered_by_exact_name(dgx_model, monkeypatch):
    seen = {}

    def fake_sh(argv, **kw):
        seen["argv"] = argv
        return _R()

    monkeypatch.setattr(dgx_model, "sh", fake_sh)
    monkeypatch.setattr(dgx_model.socket, "gethostname", lambda: dgx_model.HEAD_HOSTNAME)
    d = {"name": "x", "host": {}, "containers": {"head": ["alpha", "beta"]}}
    dgx_model.deployment_containers(d, "head")
    assert "name=^alpha$" in seen["argv"] and "name=^beta$" in seen["argv"]


def test_compose_project_is_used_when_there_is_no_containers_table(dgx_model, monkeypatch):
    seen = {}
    monkeypatch.setattr(dgx_model, "sh", lambda argv, **kw: (seen.update(argv=argv), _R())[1])
    monkeypatch.setattr(dgx_model.socket, "gethostname", lambda: dgx_model.HEAD_HOSTNAME)
    d = {"name": "x", "host": {}, "compose_project": "proj"}
    dgx_model.deployment_containers(d, "head")
    assert "label=com.docker.compose.project=proj" in seen["argv"]


def test_a_deployment_with_neither_key_is_refused_clearly(dgx_model):
    with pytest.raises(SystemExit) as e:
        dgx_model.deployment_containers({"name": "broken", "host": {}}, "head")
    assert "broken" in str(e.value)


class _R:
    returncode = 0
    stdout = ""
    stderr = ""


def test_serving_requires_this_deployments_own_containers(dgx_model, monkeypatch):
    """Two deployments can answer with the same model id; only the one with containers is serving."""
    deps = {
        "up": {"name": "up", "served_model": "same-id", "api_url": "http://x", "host": {},
               "containers": {"head": ["c"], "worker": []}},
        "down": {"name": "down", "served_model": "same-id", "api_url": "http://x", "host": {},
                 "containers": {"head": ["d"], "worker": []}},
    }
    monkeypatch.setattr(dgx_model, "served_models", lambda url, **k: ["same-id"])
    monkeypatch.setattr(dgx_model, "deployment_containers",
                        lambda d, side: [{"Names": "c"}] if d["name"] == "up" and side == "head" else [])
    monkeypatch.setattr(dgx_model, "read_state", lambda: None)
    rep = dgx_model.status(deps)
    assert rep["deployments"]["up"]["serving"] is True
    assert rep["deployments"]["down"]["serving"] is False
    assert rep["active"] == "up"


def test_served_models_retries_a_busy_api(dgx_model, monkeypatch):
    """A single slow reply from a loaded pair must not read as "nothing is serving"."""
    calls = {"n": 0}

    class Resp:
        def read(self):
            return b'{"data": [{"id": "m"}]}'

        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    def flaky(url, timeout=None):
        calls["n"] += 1
        if calls["n"] < 3:
            raise TimeoutError("busy")
        return Resp()

    monkeypatch.setattr(dgx_model.urllib.request, "urlopen", flaky)
    monkeypatch.setattr(dgx_model.time, "sleep", lambda s: None)
    assert dgx_model.served_models("http://x") == ["m"]
    assert calls["n"] == 3


def test_served_models_gives_up_and_says_nothing_rather_than_guessing(dgx_model, monkeypatch):
    monkeypatch.setattr(dgx_model.urllib.request, "urlopen",
                        lambda *a, **k: (_ for _ in ()).throw(TimeoutError("busy")))
    monkeypatch.setattr(dgx_model.time, "sleep", lambda s: None)
    assert dgx_model.served_models("http://x") is None


def test_head_containers_are_looked_up_over_ssh_when_not_on_the_head(dgx_model, monkeypatch):
    """bench and longctx run on the operator's machine; `docker ps` there finds nothing."""
    seen = {}
    monkeypatch.setattr(dgx_model, "sh", lambda argv, **kw: (seen.update(argv=argv), _R())[1])
    monkeypatch.setattr(dgx_model.socket, "gethostname", lambda: "some-laptop")
    dgx_model.deployment_containers({"name": "x", "host": {}, "containers": {"head": ["c"]}}, "head")
    assert seen["argv"][0] == "ssh", seen["argv"]


def test_head_containers_are_local_when_running_on_the_head(dgx_model, monkeypatch):
    seen = {}
    monkeypatch.setattr(dgx_model, "sh", lambda argv, **kw: (seen.update(argv=argv), _R())[1])
    monkeypatch.setattr(dgx_model.socket, "gethostname", lambda: dgx_model.HEAD_HOSTNAME + ".local")
    dgx_model.deployment_containers({"name": "x", "host": {}, "containers": {"head": ["c"]}}, "head")
    assert seen["argv"][0] == "docker", seen["argv"]


def test_remote_command_expands_the_home_tilde(dgx_model, monkeypatch, tmp_path):
    """shlex quotes the path, and a quoted ~ is a literal directory the shell never expands."""
    sent = {}
    monkeypatch.setattr(dgx_model.subprocess, "call", lambda argv: sent.update(argv=argv) or 0)
    monkeypatch.setattr(dgx_model.subprocess, "run", lambda *a, **k: None)
    monkeypatch.setattr(dgx_model, "status", lambda deps: {"active": "d", "api": {}})
    monkeypatch.setattr(dgx_model, "served_models", lambda url, **k: ["m"])
    monkeypatch.setattr(dgx_model, "STATE_DIR", tmp_path)
    deps = {"d": {"name": "d", "served_model": "m", "api_url": "http://x",
                  "api_urls": ["http://a", "http://b"], "bench_from": "head", "host": {}}}
    dgx_model.probe(deps, None, [], "llm-quickbench", "bench")
    cmd = sent["argv"][-1]
    assert "-tt" in sent["argv"], "a killed local run must take the remote sweep with it"
    assert "'~/" not in cmd, cmd
    assert "$HOME/" in cmd, cmd
    assert "http://a/v1,http://b/v1" in cmd


def test_a_remote_longctx_probe_is_told_where_prometheus_is(dgx_model, monkeypatch, tmp_path):
    """Prometheus runs where the operator is; on the head, localhost:9090 is nothing."""
    sent = {}
    monkeypatch.setattr(dgx_model.subprocess, "call", lambda argv: sent.update(argv=argv) or 0)
    monkeypatch.setattr(dgx_model.subprocess, "run", lambda *a, **k: None)
    monkeypatch.setattr(dgx_model, "status", lambda deps: {"active": "d", "api": {}})
    monkeypatch.setattr(dgx_model, "served_models", lambda url, **k: ["m"])
    monkeypatch.setattr(dgx_model, "STATE_DIR", tmp_path)
    deps = {"d": {"name": "d", "served_model": "m", "api_url": "http://x",
                  "bench_from": "head", "host": {}}}
    dgx_model.probe(deps, None, [], "llm-longctx-probe", "longctx")
    assert "--prometheus" in sent["argv"][-1]

    sent.clear()
    dgx_model.probe(deps, None, ["--prometheus", "http://given:9090"], "llm-longctx-probe", "longctx")
    assert sent["argv"][-1].count("--prometheus") == 1, "an explicit value must not be doubled"


def test_recipe_presence_looks_past_an_env_prefix(dgx_model, monkeypatch, tmp_path):
    """A start step may set environment before the recipe's script; env is not the script."""
    seen = {}

    class R:
        returncode = 0
        stdout = "ok"
        stderr = ""

    monkeypatch.setattr(dgx_model, "sh", lambda argv, **kw: (seen.update(cmd=argv[-1]), R())[1])
    d = {"name": "x", "host": {"recipe_dir": "/opt/recipe",
                               "start": [["env", "SKIP_BUILD=1", "./start.sh"]]}}
    ok, _ = dgx_model.recipe_present(d)
    assert ok
    assert "/opt/recipe/./start.sh" in seen["cmd"] or "/opt/recipe/start.sh" in seen["cmd"], seen["cmd"]


def test_recipe_presence_skips_ssh_steps(dgx_model, monkeypatch):
    """One recipe starts the worker first, over ssh; ssh is not the recipe's script."""
    seen = {}

    class R:
        returncode = 0
        stdout = "ok"
        stderr = ""

    monkeypatch.setattr(dgx_model, "sh", lambda argv, **kw: (seen.update(cmd=argv[-1]), R())[1])
    d = {"name": "x", "host": {"recipe_dir": "/opt/kit", "start": [
        ["ssh", "-o", "BatchMode=yes", "dgx02", "cd /opt/kit && start --rank 1"],
        ["bash", "-lc", "cd /opt/kit && /opt/kit/run --rank 0"]]}}
    ok, _ = dgx_model.recipe_present(d)
    assert ok and "/opt/kit" in seen["cmd"]


def test_an_all_remote_recipe_only_checks_the_directory(dgx_model, monkeypatch):
    class R:
        returncode = 0
        stdout = "ok"
        stderr = ""

    monkeypatch.setattr(dgx_model, "sh", lambda argv, **kw: R())
    d = {"name": "x", "host": {"recipe_dir": "/opt/kit",
                               "start": [["ssh", "host", "do something"]]}}
    ok, _ = dgx_model.recipe_present(d)
    assert ok


def test_a_deprecated_deployment_is_refused_before_anything_stops(dgx_model, monkeypatch, capsys):
    """Its checkout may survive the weights, so recipe_present() alone would let the switch stop the serving model."""
    called = []
    monkeypatch.setattr(dgx_model, "recipe_present", lambda d: called.append("recipe") or (True, ""))
    monkeypatch.setattr(dgx_model, "status", lambda deps: called.append("status") or {})
    d = {"name": "x", "deprecated": {"since": "2026-10-03", "reason": "weights deleted", "restore": "re-download"},
         "host": {"recipe_dir": "/nonexistent", "start": [["./start.sh"]], "stop": [["./stop.sh"]]}}
    assert dgx_model.switch({"x": d}, "x", dry=False) == 2
    assert called == []
    assert "deprecated" in capsys.readouterr().err


def test_deprecated_deployments_say_what_was_deleted_and_how_to_restore(dgx_model):
    for name, d in dgx_model.load_deployments().items():
        dep = d.get("deprecated")
        if dep:
            for key in ("since", "reason", "deleted", "restore"):
                assert dep.get(key), f"{name}: [deprecated] has no {key}"
