mod common;

use common::*;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

fn seen_json(f: &Fake) -> Vec<Value> {
    f.st.seen().iter().map(|s| serde_json::from_slice(&s.body).unwrap_or(Value::Null)).collect()
}

// --- streaming --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_reaches_the_client_frame_by_frame() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let body = serde_json::to_vec(&json!({"model": QWEN, "stream": true})).unwrap();
    let resp =
        open(env.port(), "POST", "/v1/chat/completions", &[("content-type", "application/json"), ("x-fake-mode", "gate_sse")], Some(body))
            .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut inc = resp.into_body();
    // The upstream is parked on its gate after the first event: that event must already be here.
    let first = tokio::time::timeout(Duration::from_secs(3), inc.frame()).await.expect("first frame not delayed").unwrap().unwrap();
    assert!(first.data_ref().unwrap().starts_with(b"data: {\"choices\""));
    assert!(!a.st.sent_last.load(Ordering::SeqCst), "upstream already sent the last event");
    a.st.open_gate();
    let rest = inc.collect().await.unwrap().to_bytes();
    assert!(rest.ends_with(b"data: [DONE]\n\n"));
    let row = env.wait_rows(1).await.pop().unwrap();
    assert_eq!(row["outcome"], "ok");
    assert_eq!(row["stream"], true);
    assert_eq!((row["prompt_tokens"].as_i64(), row["completion_tokens"].as_i64()), (Some(1), Some(2)));
    assert!(row["reasoning_tokens"].is_null());
    assert!(row["ttft_s"].as_f64().unwrap() >= row["headers_s"].as_f64().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_from_chat_json_chat_sse_and_responses_sse() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let p = env.port();
    post(p, "/v1/chat/completions", "json,reasoning=5", &chat(QWEN)).await;
    post(p, "/v1/chat/completions", "sse_chat", &json!({"model": QWEN, "stream": true})).await;
    post(p, "/v1/responses", "sse_resp", &json!({"model": QWEN, "stream": true})).await;
    post(p, "/v1/chat/completions", "json", &chat(QWEN)).await;
    let rows = env.wait_rows(4).await;
    let t = |r: &Value| (r["prompt_tokens"].as_i64(), r["completion_tokens"].as_i64(), r["reasoning_tokens"].as_i64());
    assert_eq!(t(&rows[0]), (Some(11), Some(7), Some(5)));
    assert_eq!(t(&rows[1]), (Some(5), Some(3), Some(2)));
    assert_eq!(t(&rows[2]), (Some(20), Some(9), Some(4)));
    assert_eq!(t(&rows[3]), (Some(11), Some(7), None)); // qs-style: not reported -> stays null
    assert!(rows[3]["reasoning_tokens"].is_null());
    let m = env.proxy.app.metrics.get(
        "lb_reasoning_tokens_total",
        &[("served_model", QWEN), ("deployment", "qwen3.8-flash-next-single-x2"), ("node", "fleet"), ("profile", "default")],
    );
    assert_eq!(m, 11.0);
}

// --- rewrites ---------------------------------------------------------------------------------

/// A codex (0.157-style) Responses request, as it reached strip_include_proxy.py on 2026-10-05.
fn codex_request() -> Value {
    let f = |name: &str, desc: &str| {
        json!({"type": "function", "name": name, "description": desc, "strict": false,
               "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"],
                              "additionalProperties": false}})
    };
    json!({
        "model": GLM,
        "instructions": "You are Codex, a coding agent. Follow the user's instructions.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<permissions>workspace-write</permissions>"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Build a voxel garden. Temperature 0.7, ünïcødé ✓"}]},
            {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls -la\"}", "call_id": "call_1"},
            {"type": "function_call_output", "call_id": "call_1", "output": "total 0\n"}
        ],
        "tools": [
            f("shell", "Runs a shell command"),
            f("apply_patch", "Applies a patch"),
            f("update_plan", "Updates the plan"),
            f("view_image", "Attaches an image"),
            {"type": "namespace", "name": "multi_agent_v1", "tools": [f("spawn_agent", "Spawns"), f("wait_agent", "Waits")]},
            {"type": "web_search", "external_web_access": false}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "0199a6c4-7f1e-7c33-9d2e-5b1f0e4a2c11",
        "client_metadata": {"originator": "codex_exec", "session_id": "s-1"},
        "max_output_tokens": 32768,
        "temperature": 0.7
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_codex_request_loses_exactly_include_and_non_function_tools() {
    let glm = Fake::start(&[GLM]).await;
    let qwen = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &glm.url), ("dgx02", &qwen.url)], Opts::default()).await;
    let req = codex_request();
    let r = post(env.port(), "/v1/responses", "sse_resp", &req).await;
    assert_eq!(r.status, 200);
    let mut expected = req.clone();
    expected.as_object_mut().unwrap().shift_remove("include");
    expected["tools"] = Value::Array(req["tools"].as_array().unwrap()[..4].to_vec());
    let got = seen_json(&glm).pop().unwrap();
    assert_eq!(got, expected);
    // order of keys is kept too (preserve_order): compare the serialisations
    assert_eq!(serde_json::to_string(&got).unwrap(), serde_json::to_string(&expected).unwrap());
    let row = env.wait_rows(1).await.pop().unwrap();
    assert_eq!(row["rewrites"], json!(["drop-include", "function-tools-only"]));
    assert_eq!(row["deployment"], "glm-5.3-flash-tensorfold");
    assert_eq!((row["model_in"].as_str(), row["model_out"].as_str()), (Some(GLM), Some(GLM)));
    assert_eq!(
        env.proxy
            .app
            .metrics
            .get("lb_rewrites_total", &[("rule", "drop-include"), ("deployment", "glm-5.3-flash-tensorfold"), ("profile", "default")]),
        1.0
    );

    // The same request to a deployment without rules goes through byte for byte.
    let mut raw = serde_json::to_vec_pretty(&json!({"model": QWEN, "include": ["x"], "tools": req["tools"]})).unwrap();
    raw.extend_from_slice(b"\n  ");
    let r = send(env.port(), "POST", "/v1/responses", &[("content-type", "application/json"), ("x-fake-mode", "json")], Some(raw.clone()))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(qwen.st.seen().pop().unwrap().body, raw);
    assert_eq!(env.wait_rows(2).await[1]["rewrites"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_merge_keeps_other_kwargs_by_port_and_by_path() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let body = json!({"model": QWEN, "chat_template_kwargs": {"foo": 1, "enable_thinking": false}});
    let r = post(env.proxy.port("think"), "/v1/chat/completions", "json", &body).await;
    assert_eq!(r.json()["echo"]["chat_template_kwargs"], json!({"foo": 1, "enable_thinking": true, "thinking": true}));
    let r = post(env.port(), "/p/nothink/v1/chat/completions", "json", &body).await;
    assert_eq!(r.json()["echo"]["chat_template_kwargs"], json!({"foo": 1, "enable_thinking": false, "thinking": false}));
    assert_eq!(r.json()["path"], "/v1/chat/completions");
    let r = post(env.port(), "/v1/chat/completions", "json", &body).await;
    assert_eq!(r.json()["echo"]["chat_template_kwargs"], json!({"foo": 1, "enable_thinking": false}));
    let rows = env.wait_rows(3).await;
    assert_eq!((rows[0]["profile"].as_str(), rows[0]["rewrites"].clone()), (Some("think"), json!(["think"])));
    assert_eq!((rows[1]["profile"].as_str(), rows[1]["rewrites"].clone()), (Some("nothink"), json!(["nothink"])));
    assert_eq!((rows[2]["profile"].as_str(), rows[2]["rewrites"].clone()), (Some("default"), json!([])));
    let r = post(env.port(), "/p/nope/v1/chat/completions", "json", &body).await;
    assert_eq!(r.status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alias_is_renamed_to_the_served_model() {
    let a = Fake::start(&[GLM]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let r = post(env.port(), "/v1/chat/completions", "json", &chat("glm-5.3-flash")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["echo"]["model"], GLM);
    let row = env.wait_rows(1).await.pop().unwrap();
    assert_eq!((row["model_in"].as_str(), row["model_out"].as_str()), (Some("glm-5.3-flash"), Some(GLM)));
    assert_eq!(row["rewrites"], json!(["rename_model"]));
}

// --- routing ----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_model_goes_to_the_only_eligible_deployment_unchanged() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let r = post(env.port(), "/v1/chat/completions", "json", &chat("gpt-whatever")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["echo"]["model"], "gpt-whatever");
    let row = env.wait_rows(1).await.pop().unwrap();
    assert_eq!(row["deployment"], "qwen3.8-flash-next-single-x2");
    assert_eq!((row["model_in"].as_str(), row["model_out"].as_str()), (Some("gpt-whatever"), Some("gpt-whatever")));
    // no model at all behaves the same
    let r = post(env.port(), "/v1/chat/completions", "json", &json!({"messages": []})).await;
    assert_eq!(r.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_model_with_two_deployments_up_is_404_with_the_list() {
    let a = Fake::start(&[QWEN]).await;
    let b = Fake::start(&[GLM]).await;
    let env = start(&[("dgx01", &a.url), ("dgx02", &b.url)], Opts::default()).await;
    let r = post(env.port(), "/v1/chat/completions", "json", &chat("gpt-whatever")).await;
    assert_eq!(r.status, 404);
    let models: Vec<String> = r.json()["models"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(models, vec![GLM.to_string(), "glm-5.3-flash".into(), QWEN.into()]);
    assert_eq!(env.wait_rows(1).await[0]["outcome"], "unknown_model");
    assert!(a.st.seen().is_empty() && b.st.seen().is_empty());
    assert_eq!(post(env.port(), "/v1/chat/completions", "json", &chat(QWEN)).await.header("x-dgx-lb-node").as_deref(), Some("dgx01"));
    assert_eq!(post(env.port(), "/v1/chat/completions", "json", &chat(GLM)).await.header("x-dgx-lb-node").as_deref(), Some("dgx02"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn least_outstanding_with_cap_and_least_recently_picked_tie_break() {
    let a = Fake::start(&[QWEN]).await;
    let b = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url), ("dgx02", &b.url)], Opts::default()).await;
    let port = env.port();
    // sequential: the tie goes to the least recently picked node -> strict alternation
    let mut nodes = vec![];
    for _ in 0..4 {
        nodes.push(post(port, "/v1/chat/completions", "json", &chat(QWEN)).await.header("x-dgx-lb-node").unwrap());
    }
    assert_eq!(nodes, ["dgx01", "dgx02", "dgx01", "dgx02"]);
    // concurrent: 4 held requests, cap 2 per node -> 2 + 2
    let hs: Vec<_> =
        (0..4).map(|_| tokio::spawn(async move { post(port, "/v1/chat/completions", "json,wait", &chat(QWEN)).await })).collect();
    assert!(wait_for(|| a.st.inflight.load(Ordering::SeqCst) == 2 && b.st.inflight.load(Ordering::SeqCst) == 2, 5.0).await);
    a.st.open_gate();
    b.st.open_gate();
    for h in hs {
        assert_eq!(h.await.unwrap().status, 200);
    }
    assert_eq!((a.st.max_inflight.load(Ordering::SeqCst), b.st.max_inflight.load(Ordering::SeqCst)), (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_backends_queue_in_fifo_order_and_overflow_is_queue_full() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts { qwen_cap: 1, max_depth: 2, ..Opts::default() }).await;
    let port = env.port();
    let pool = env.proxy.app.pool.clone();
    let mut hs = vec![];
    for i in 0..3 {
        let body = json!({"model": QWEN, "n": i});
        hs.push(tokio::spawn(async move { post(port, "/v1/chat/completions", "json,wait", &body).await }));
        if i == 0 {
            assert!(wait_for(|| a.st.inflight.load(Ordering::SeqCst) == 1, 5.0).await);
        } else {
            let p = pool.clone();
            assert!(wait_for(move || p.queue_len() == i, 5.0).await);
        }
    }
    let r = post(port, "/v1/chat/completions", "json", &chat(QWEN)).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"], "queue_full");
    let h = get(port, "/health").await.json();
    assert_eq!(h["queue_depth"], 2);
    assert_eq!(h["deployments"]["qwen3.8-flash-next-single-x2"]["queue_depth"], 2);
    tokio::time::sleep(Duration::from_millis(200)).await;
    a.st.open_gate();
    for h in hs {
        assert_eq!(h.await.unwrap().status, 200);
    }
    let order: Vec<i64> = seen_json(&a).iter().map(|v| v["n"].as_i64().unwrap()).collect();
    assert_eq!(order, [0, 1, 2]);
    assert_eq!(a.st.max_inflight.load(Ordering::SeqCst), 1);
    let rows = env.wait_rows(4).await;
    assert_eq!(rows[0]["outcome"], "queue_full");
    let waited: Vec<f64> = rows[1..].iter().map(|r| r["queue_wait_s"].as_f64().unwrap()).collect();
    assert!(waited.iter().filter(|w| **w > 0.1).count() >= 2, "{waited:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_503_on_the_other_backend_but_not_400_nor_after_the_first_byte() {
    let a = Fake::start(&[QWEN]).await;
    let b = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url), ("dgx02", &b.url)], Opts::default()).await;
    a.st.force(Some("status=503"));
    let r = post(env.port(), "/v1/chat/completions", "json", &chat(QWEN)).await;
    assert_eq!((r.status, r.header("x-dgx-lb-node").as_deref()), (200, Some("dgx02")));
    let row = env.wait_rows(1).await.pop().unwrap();
    assert_eq!((row["attempts"].as_u64(), row["outcome"].as_str(), row["node"].as_str()), (Some(2), Some("retried_ok"), Some("dgx02")));
    assert_eq!(row["errors"], json!(["dgx01: HTTP 503"]));
    let lab =
        |n: &'static str| [("served_model", QWEN), ("deployment", "qwen3.8-flash-next-single-x2"), ("node", n), ("profile", "default")];
    assert_eq!(env.proxy.app.metrics.get("lb_retries_total", &lab("dgx01")), 1.0);
    assert_eq!(env.proxy.app.metrics.get("lb_retries_total", &lab("fleet")), 1.0);
    a.st.force(None);
    env.probe().await;

    let r = post(env.port(), "/v1/chat/completions", "status=400", &chat(QWEN)).await;
    assert_eq!(r.status, 400);
    let row = env.wait_rows(2).await.pop().unwrap();
    assert_eq!((row["attempts"].as_u64(), row["status"].as_u64()), (Some(1), Some(400)));

    let before = a.st.seen().len() + b.st.seen().len();
    let r = post(env.port(), "/v1/chat/completions", "midstream", &json!({"model": QWEN, "stream": true})).await;
    assert_eq!(r.status, 200);
    assert!(r.broken, "the client must see a broken stream");
    assert!(r.body.windows(3).any(|w| w == b"\"a\""));
    let row = env.wait_rows(3).await.pop().unwrap();
    assert_eq!((row["outcome"].as_str(), row["attempts"].as_u64()), (Some("midstream_broken"), Some(1)));
    assert_eq!(a.st.seen().len() + b.st.seen().len(), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_refused_is_retried_then_ejected_and_readmitted_by_health() {
    let a = Fake::start(&[QWEN]).await;
    let b = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url), ("dgx02", &b.url)], Opts::default()).await;
    let port_a = a.port;
    let st_a = a.st.clone();
    a.stop();
    drop(a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = post(env.port(), "/v1/chat/completions", "json", &chat(QWEN)).await;
    assert_eq!((r.status, r.header("x-dgx-lb-node").as_deref()), (200, Some("dgx02")));
    let row = env.wait_rows(1).await.pop().unwrap();
    assert_eq!((row["attempts"].as_u64(), row["outcome"].as_str()), (Some(2), Some("retried_ok")));
    env.probe().await; // second failure -> ejected
    assert!(!get_backend_up(&env, "dgx01").await);
    for _ in 0..3 {
        assert_eq!(post(env.port(), "/v1/chat/completions", "json", &chat(QWEN)).await.header("x-dgx-lb-node").as_deref(), Some("dgx02"));
    }
    let _a2 = Fake::start_with(st_a, port_a).await;
    env.probe().await;
    assert!(!get_backend_up(&env, "dgx01").await, "one good probe is not enough");
    env.probe().await;
    assert!(get_backend_up(&env, "dgx01").await);
}

async fn get_backend_up(env: &Env, name: &str) -> bool {
    get(env.port(), "/health").await.json()["backends"][name]["up"].as_bool().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deployment_switch_is_picked_up_by_the_probe_without_restart() {
    let a = Fake::start(&[QWEN]).await;
    let b = Fake::start(&[GLM]).await;
    let env = start(&[("dgx01", &a.url), ("dgx02", &b.url)], Opts::default()).await;
    for _ in 0..4 {
        assert_eq!(post(env.port(), "/v1/chat/completions", "json", &chat(QWEN)).await.header("x-dgx-lb-node").as_deref(), Some("dgx01"));
    }
    assert!(b.st.seen().is_empty());
    // dgx-model switches dgx02 to the qwen replica
    b.st.set_models(&[QWEN]);
    env.probe().await;
    let mut nodes = std::collections::HashSet::new();
    for _ in 0..4 {
        nodes.insert(post(env.port(), "/v1/chat/completions", "json", &chat(QWEN)).await.header("x-dgx-lb-node").unwrap());
    }
    assert_eq!(nodes.len(), 2);
    let r = post(env.port(), "/v1/chat/completions", "json", &chat(GLM)).await;
    assert_eq!((r.status, r.json()["error"].as_str()), (503, Some("no_upstream")));
    let ids: Vec<Value> = get(env.port(), "/v1/models").await.json()["data"].as_array().unwrap().iter().map(|m| m["id"].clone()).collect();
    assert_eq!(ids, vec![json!(QWEN)]);
    // an unknown model is no longer ambiguous: only qwen is up
    assert_eq!(post(env.port(), "/v1/chat/completions", "json", &chat("x")).await.status, 200);
}

// --- timeouts / cancellation ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeouts_have_their_outcomes() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts { timeouts: (1.0, 0.5, 0.5, 1.5), eject_after: 5, qwen_cap: 1, ..Opts::default() }).await;
    let p = env.port();
    let r = post(p, "/v1/chat/completions", "slow_first=1.5", &chat(QWEN)).await;
    assert_eq!(r.status, 504);
    assert_eq!(env.wait_rows(1).await[0]["outcome"], "timeout_first_byte");
    post(p, "/v1/chat/completions", "slow_idle=1.2", &json!({"model": QWEN, "stream": true})).await;
    let row = env.wait_rows(2).await.pop().unwrap();
    assert_eq!(row["outcome"], "timeout_idle");
    assert!(row["ttft_s"].is_number(), "the first event arrived before the stall");
    post(p, "/v1/chat/completions", "drip=3", &json!({"model": QWEN, "stream": true})).await;
    assert_eq!(env.wait_rows(3).await[2]["outcome"], "timeout_total");
    // queued past first_byte -> 504 timeout_first_byte without touching the upstream
    // the holder streams (headers sent, so its own first_byte is satisfied) and keeps the only slot
    let h = tokio::spawn(async move { post(p, "/v1/chat/completions", "drip=1", &json!({"model": QWEN, "stream": true})).await });
    assert!(wait_for(|| a.st.seen().len() == 4, 5.0).await);
    let seen = a.st.seen().len();
    let r = post(p, "/v1/chat/completions", "json", &chat(QWEN)).await;
    assert_eq!((r.status, r.json()["error"].as_str()), (504, Some("timeout_first_byte")));
    assert_eq!(a.st.seen().len(), seen);
    assert_eq!(h.await.unwrap().status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_refuses_short_timeouts_bp60() {
    let a = Fake::start(&[QWEN]).await;
    let dir = tmpdir();
    for (fb, total, ok) in [(900.0, 20.0, false), (20.0, 3900.0, false), (900.0, 3900.0, true)] {
        let o = Opts { timeouts: (5.0, fb, 300.0, total), ..Opts::default() };
        let cfg = dgx_proxy::Config::from_toml(&config_text(&dir, &[("dgx01", &a.url)], &o)).unwrap();
        assert_eq!(dgx_proxy::start(cfg, false).await.is_ok(), ok, "first_byte={fb} total={total}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_cancels_the_upstream() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    // mid-stream
    let body = serde_json::to_vec(&json!({"model": QWEN, "stream": true})).unwrap();
    let resp = open(
        env.port(),
        "POST",
        "/v1/chat/completions",
        &[("content-type", "application/json"), ("x-fake-mode", "drip=8")],
        Some(body.clone()),
    )
    .await;
    let mut inc = resp.into_body();
    inc.frame().await.unwrap().unwrap();
    drop(inc);
    assert!(wait_for(|| a.st.cancelled.load(Ordering::SeqCst) == 1, 3.0).await, "upstream kept streaming");
    assert_eq!(env.wait_rows(1).await[0]["outcome"], "client_gone");
    assert_eq!(get(env.port(), "/health").await.json()["backends"]["dgx01"]["inflight"], 0);
    // before the response headers (upstream still thinking)
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", env.port())).await.unwrap();
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nX-Fake-Mode: slow_first=8\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(&body).await.unwrap();
    assert!(wait_for(|| a.st.inflight.load(Ordering::SeqCst) == 1, 3.0).await);
    drop(s);
    assert!(wait_for(|| a.st.cancelled.load(Ordering::SeqCst) == 2, 3.0).await, "upstream not cancelled before headers");
    assert_eq!(env.wait_rows(2).await[1]["outcome"], "client_gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_body_is_413_before_reading_it() {
    let a = Fake::start(&[QWEN]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", env.port())).await.unwrap();
    s.write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 200000000\r\n\r\n{")
        .await
        .unwrap();
    let mut buf = vec![0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf)).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 413"));
    assert_eq!(env.wait_rows(1).await[0]["outcome"], "body_too_large");
}

// --- endpoints / metrics / ledger -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_health_and_metrics() {
    let a = Fake::start(&[QWEN]).await;
    let b = Fake::start(&[QWEN]).await;
    let g = Fake::start(&[GLM]).await;
    let env = start(&[("dgx01", &a.url), ("dgx02", &b.url), ("dgx03", &g.url)], Opts::default()).await;
    let p = env.port();
    let ids: Vec<String> = get(p, "/p/think/v1/models").await.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [GLM, "glm-5.3-flash", QWEN]);
    let h = get(p, "/health").await;
    assert_eq!(h.status, 200);
    let h = h.json();
    assert_eq!(h["eligible"], 2);
    assert_eq!(h["deployments"]["qwen3.8-flash-next-single-x2"]["backends"], json!(["dgx01", "dgx02"]));
    assert_eq!(h["deployments"]["glm-5.3-flash-tensorfold"]["backends"], json!(["dgx03"]));
    assert_eq!(h["backends"]["dgx03"]["models"], json!([GLM]));

    for mode in ["json", "json,reasoning=3", "sse_chat", "json", "status=400"] {
        post(p, "/v1/chat/completions", mode, &json!({"model": QWEN, "stream": mode.starts_with("sse")})).await;
    }
    post(p, "/v1/responses", "sse_resp", &json!({"model": GLM, "include": ["x"], "stream": true})).await;
    env.wait_rows(6).await;
    let text = get(p, "/metrics").await.body;
    let m = parse_metrics(std::str::from_utf8(&text).unwrap());
    let fleet_is_sum = |name: &str| {
        let mut groups: HashMap<Vec<(String, String)>, (f64, f64)> = HashMap::new();
        for (n, lab, v) in &m {
            if n != name {
                continue;
            }
            let mut key: Vec<(String, String)> = lab.iter().filter(|(k, _)| *k != "node").map(|(k, v)| (k.clone(), v.clone())).collect();
            key.sort();
            let e = groups.entry(key).or_default();
            if lab["node"] == "fleet" {
                e.0 += v;
            } else {
                e.1 += v;
            }
        }
        assert!(!groups.is_empty(), "{name} missing");
        for (k, (fleet, sum)) in groups {
            assert!((fleet - sum).abs() < 1e-9, "{name} {k:?}: fleet {fleet} != {sum}");
        }
    };
    for name in [
        "lb_requests_total",
        "lb_prompt_tokens_total",
        "lb_generation_tokens_total",
        "lb_reasoning_tokens_total",
        "lb_node_up",
        "lb_requests_inflight",
        "lb_vllm_running",
    ] {
        fleet_is_sum(name);
    }
    let one = |name: &str, want: &[(&str, &str)]| -> Vec<f64> {
        m.iter()
            .filter(|(n, lab, _)| n == name && want.iter().all(|(k, v)| lab.get(*k).map(String::as_str) == Some(*v)))
            .map(|x| x.2)
            .collect()
    };
    assert_eq!(one("lb_rewrites_total", &[("rule", "drop-include"), ("deployment", "glm-5.3-flash-tensorfold")]), [1.0]);
    assert_eq!(one("lb_requests_total", &[("node", "fleet"), ("deployment", "qwen3.8-flash-next-single-x2"), ("outcome", "ok")]), [5.0]);
    assert_eq!(one("lb_reasoning_tokens_total", &[("node", "fleet"), ("served_model", QWEN)]), [5.0]);
    assert_eq!(one("lb_node_up", &[("node", "fleet"), ("deployment", "qwen3.8-flash-next-single-x2")]), [2.0]);
    assert_eq!(one("lb_node_up", &[("node", "dgx03"), ("deployment", "qwen3.8-flash-next-single-x2")]), [0.0]);
    assert_eq!(one("lb_node_cap", &[("node", "fleet"), ("deployment", "qwen3.8-flash-next-single-x2")]), [4.0]);
    assert_eq!(one("lb_queue_depth", &[("node", "fleet"), ("deployment", "glm-5.3-flash-tensorfold")]), [0.0]);
    assert_eq!(one("lb_vllm_generation_tokens_total", &[("deployment", "qwen3.8-flash-next-single-x2")]).len(), 2); // per node, no fleet
    assert!(one("lb_ttft_seconds_count", &[("node", "fleet")]).iter().sum::<f64>() >= 6.0);
    assert!(m.iter().any(|(n, l, _)| n == "lb_ttft_seconds_bucket" && l.get("le").map(String::as_str) == Some("+Inf")));

    // every backend gone -> /health and /v1/models are 503, requests are no_upstream
    for f in [&a, &b, &g] {
        f.stop();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    env.probe().await;
    env.probe().await;
    assert_eq!(get(p, "/health").await.status, 503);
    assert_eq!(get(p, "/v1/models").await.status, 503);
    let r = post(p, "/v1/chat/completions", "json", &chat(QWEN)).await;
    assert_eq!(r.status, 503);
    assert_eq!(env.wait_rows(7).await[6]["outcome"], "no_upstream");
    let text = String::from_utf8(get(p, "/metrics").await.body.to_vec()).unwrap();
    let m2 = parse_metrics(&text);
    let up: Vec<_> = m2.iter().filter(|(n, l, _)| n == "lb_node_up" && l["node"] == "fleet").map(|x| x.2).collect();
    assert_eq!(up, [0.0, 0.0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ledger_row_fields_request_id_dispatch_and_no_body() {
    let a = Fake::start(&[GLM]).await;
    let env = start(&[("dgx01", &a.url)], Opts::default()).await;
    let marker = "SECRET-PROMPT-MARKER-7f3a";
    let body = json!({"model": "glm-5.3-flash", "messages": [{"role": "user", "content": marker}], "include": [marker]});
    let r = post_h(env.port(), "/d/bb2-abc123/p/nothink/v1/responses?x=1", "json", &body, &[("x-request-id", "rid-42")]).await;
    assert_eq!((r.status, r.header("x-request-id").as_deref()), (200, Some("rid-42")));
    let seen = a.st.seen().pop().unwrap();
    assert_eq!((seen.path.as_str(), seen.headers.get("x-request-id").map(String::as_str)), ("/v1/responses?x=1", Some("rid-42")));
    let row = env.wait_rows(1).await.pop().unwrap();
    let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "ts",
            "rid",
            "profile",
            "dispatch_id",
            "method",
            "path",
            "stream",
            "deployment",
            "model_in",
            "model_out",
            "rewrites",
            "node",
            "attempts",
            "status",
            "outcome",
            "queue_wait_s",
            "headers_s",
            "ttft_s",
            "wall_s",
            "bytes",
            "prompt_tokens",
            "completion_tokens",
            "reasoning_tokens",
            "errors"
        ]
    );
    assert_eq!(row["rid"], "rid-42");
    assert_eq!(row["dispatch_id"], "bb2-abc123");
    assert_eq!(row["profile"], "nothink");
    assert_eq!(row["path"], "/v1/responses?x=1");
    assert_eq!(row["method"], "POST");
    assert_eq!(row["stream"], false);
    assert_eq!(row["node"], "dgx01");
    assert_eq!(row["status"], 200);
    assert_eq!(row["outcome"], "ok");
    assert_eq!(row["rewrites"], json!(["nothink", "drop-include", "rename_model"]));
    assert_eq!((row["model_in"].as_str(), row["model_out"].as_str()), (Some("glm-5.3-flash"), Some(GLM)));
    assert!(row["bytes"].as_u64().unwrap() > 0);
    let all: String = std::fs::read_dir(&env.ledger).unwrap().map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap()).collect();
    assert!(!all.contains(marker), "the ledger must not contain request content");
    assert!(!all.contains("\"hi\""), "the ledger must not contain response content");
    let r = post(env.port(), "/v1/chat/completions", "json", &chat(GLM)).await;
    let rid = r.header("x-request-id").unwrap();
    assert!(rid.len() == 16 && rid.chars().all(|c| c.is_ascii_hexdigit()), "{rid}");
    assert_eq!(env.wait_rows(2).await[1]["rid"], rid.as_str());
}
