//! Test fixtures: fake OpenAI-compatible upstreams written on raw TCP (so a test controls every
//! byte and every pause), a small client, and config/ledger helpers.
//!
//! The fake's behaviour is chosen per request by the X-Fake-Mode header (the proxy forwards
//! client headers), or per server by `FakeState::force`. Options are comma separated:
//!   json (default) | reasoning=N | status=N | hold=S | slow_first=S | wait | sse_chat | sse_resp
//!   | midstream | slow_idle=S | drip=S | gate_sse
//! `wait` and `gate_sse` block on the fake's gate (`open_gate`). While a fake waits before its
//! response headers it watches the socket, so a proxy that closes the connection is counted in
//! `cancelled` (as is a failed write mid-stream).
#![allow(dead_code)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub const QWEN: &str = "qwen3.8-flash-next-single";
pub const GLM: &str = "GLM-5.3-Flash-EXL3-TensorFold";

#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct FakeState {
    pub models: Mutex<Vec<String>>,
    pub force: Mutex<Option<String>>,
    pub inflight: AtomicUsize,
    pub max_inflight: AtomicUsize,
    pub seen: Mutex<Vec<Seen>>,
    pub cancelled: AtomicUsize,
    pub finished: AtomicUsize,
    pub sent_last: AtomicBool,
    gate: watch::Sender<bool>,
}

impl FakeState {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    pub fn set_models(&self, m: &[&str]) {
        *self.models.lock().unwrap() = m.iter().map(|s| s.to_string()).collect();
    }
    pub fn force(&self, mode: Option<&str>) {
        *self.force.lock().unwrap() = mode.map(String::from);
    }
    pub fn open_gate(&self) {
        let _ = self.gate.send(true);
    }
}

pub struct Fake {
    pub port: u16,
    pub url: String,
    pub st: Arc<FakeState>,
    task: JoinHandle<()>,
}

impl Fake {
    pub async fn start(models: &[&str]) -> Fake {
        Self::start_on(models, 0).await
    }

    pub async fn start_on(models: &[&str], port: u16) -> Fake {
        let (gate, _) = watch::channel(false);
        let st = Arc::new(FakeState {
            models: Mutex::new(models.iter().map(|s| s.to_string()).collect()),
            force: Mutex::new(None),
            inflight: AtomicUsize::new(0),
            max_inflight: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            cancelled: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            sent_last: AtomicBool::new(false),
            gate,
        });
        Self::start_with(st, port).await
    }

    pub async fn start_with(st: Arc<FakeState>, port: u16) -> Fake {
        let l = TcpListener::bind(("127.0.0.1", port)).await.expect("bind fake");
        let port = l.local_addr().unwrap().port();
        let s2 = st.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = l.accept().await else { continue };
                let st = s2.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, st).await;
                });
            }
        });
        Fake { port, url: format!("http://127.0.0.1:{port}"), st, task }
    }

    /// Stop listening (connections are refused afterwards).
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(s: &mut TcpStream) -> std::io::Result<Option<(String, String, HashMap<String, String>, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 65536];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    if headers.get("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        // de-chunk (only used when the proxy streams a request body through)
        let mut raw = body;
        loop {
            if raw.ends_with(b"0\r\n\r\n") {
                break;
            }
            let n = s.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        body = Vec::new();
        let mut i = 0;
        while let Some(off) = raw[i..].windows(2).position(|w| w == b"\r\n") {
            let size = usize::from_str_radix(std::str::from_utf8(&raw[i..i + off]).unwrap_or("0").trim(), 16).unwrap_or(0);
            i += off + 2;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&raw[i..i + size]);
            i += size + 2;
        }
    } else {
        let want: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        while body.len() < want {
            let n = s.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
    }
    Ok(Some((method, path, headers, body)))
}

async fn write_json(s: &mut TcpStream, code: u16, v: &Value) -> std::io::Result<()> {
    let b = serde_json::to_vec(v).unwrap();
    let head = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
    s.write_all(head.as_bytes()).await?;
    s.write_all(&b).await?;
    s.flush().await
}

async fn sse_head(s: &mut TcpStream) -> std::io::Result<()> {
    s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await?;
    s.flush().await
}

async fn chunk(s: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    s.write_all(format!("{:x}\r\n", data.len()).as_bytes()).await?;
    s.write_all(data).await?;
    s.write_all(b"\r\n").await?;
    s.flush().await
}

async fn end(s: &mut TcpStream) -> std::io::Result<()> {
    s.write_all(b"0\r\n\r\n").await?;
    s.flush().await
}

/// Sleep, or return false early when the peer closes the connection.
async fn pause_watching(s: &mut TcpStream, d: Duration) -> bool {
    let mut b = [0u8; 1];
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        r = s.read(&mut b) => !matches!(r, Ok(0) | Err(_)),
    }
}

async fn gate_watching(s: &mut TcpStream, mut rx: watch::Receiver<bool>) -> bool {
    let mut b = [0u8; 1];
    tokio::select! {
        r = rx.wait_for(|v| *v) => r.is_ok(),
        r = s.read(&mut b) => !matches!(r, Ok(0) | Err(_)),
    }
}

const CHAT_A: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n";

async fn handle(mut s: TcpStream, st: Arc<FakeState>) -> std::io::Result<()> {
    let _ = s.set_nodelay(true);
    let Some((method, path, headers, body)) = read_request(&mut s).await? else { return Ok(()) };
    if method == "GET" && path == "/v1/models" {
        let data: Vec<Value> = st.models.lock().unwrap().iter().map(|m| json!({"id": m, "object": "model"})).collect();
        return write_json(&mut s, 200, &json!({"object": "list", "data": data})).await;
    }
    if method == "GET" && path == "/metrics" {
        let models = st.models.lock().unwrap().clone();
        let m = models.first().cloned().unwrap_or_default();
        let t = format!(
            "vllm:num_requests_running{{model_name=\"{m}\"}} {}\nvllm:num_requests_waiting{{model_name=\"{m}\"}} 0\n\
             vllm:generation_tokens_total{{model_name=\"{m}\"}} 42\n",
            st.inflight.load(Ordering::SeqCst)
        );
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", t.len());
        s.write_all(head.as_bytes()).await?;
        return s.write_all(t.as_bytes()).await;
    }
    st.seen.lock().unwrap().push(Seen { path: path.clone(), headers: headers.clone(), body: body.clone() });
    let mode = st.force.lock().unwrap().clone().or_else(|| headers.get("x-fake-mode").cloned()).unwrap_or_else(|| "json".into());
    let n = st.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    st.max_inflight.fetch_max(n, Ordering::SeqCst);
    let r = serve(&mut s, &st, &mode, &path, &body).await;
    st.inflight.fetch_sub(1, Ordering::SeqCst);
    match r {
        Ok(true) => {
            st.finished.fetch_add(1, Ordering::SeqCst);
        }
        Ok(false) | Err(_) => {
            st.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Ok(true) = served, Ok(false)/Err = the peer went away.
async fn serve(s: &mut TcpStream, st: &FakeState, mode: &str, path: &str, body: &[u8]) -> std::io::Result<bool> {
    let opts: HashMap<&str, &str> = mode.split(',').map(|p| p.split_once('=').unwrap_or((p, "1"))).collect();
    let secs = |k: &str| opts.get(k).and_then(|v| v.parse::<f64>().ok()).map(Duration::from_secs_f64);
    if let Some(d) = secs("hold") {
        tokio::time::sleep(d).await;
    }
    if let Some(d) = secs("slow_first") {
        if !pause_watching(s, d).await {
            return Ok(false);
        }
    }
    if opts.contains_key("wait") && !gate_watching(s, st.gate.subscribe()).await {
        return Ok(false);
    }
    if let Some(code) = opts.get("status") {
        write_json(s, code.parse().unwrap(), &json!({"error": "forced"})).await?;
        return Ok(true);
    }
    if opts.contains_key("sse_chat") {
        sse_head(s).await?;
        chunk(s, CHAT_A).await?;
        chunk(s, b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n").await?;
        chunk(s, b"data: [DONE]\n\n").await?;
        end(s).await?;
        return Ok(true);
    }
    if opts.contains_key("sse_resp") {
        sse_head(s).await?;
        chunk(s, b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n").await?;
        chunk(s, b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":9,\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n").await?;
        end(s).await?;
        return Ok(true);
    }
    if opts.contains_key("gate_sse") {
        sse_head(s).await?;
        chunk(s, CHAT_A).await?;
        if !gate_watching(s, st.gate.subscribe()).await {
            return Ok(false);
        }
        chunk(s, b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n").await?;
        st.sent_last.store(true, Ordering::SeqCst);
        chunk(s, b"data: [DONE]\n\n").await?;
        end(s).await?;
        return Ok(true);
    }
    if opts.contains_key("midstream") {
        sse_head(s).await?;
        chunk(s, CHAT_A).await?;
        chunk(s, b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n").await?;
        s.shutdown().await?;
        return Ok(true);
    }
    if let Some(d) = secs("slow_idle") {
        sse_head(s).await?;
        chunk(s, CHAT_A).await?;
        if !pause_watching(s, d).await {
            return Ok(false);
        }
        chunk(s, b"data: [DONE]\n\n").await?;
        end(s).await?;
        return Ok(true);
    }
    if let Some(d) = secs("drip") {
        sse_head(s).await?;
        let until = tokio::time::Instant::now() + d;
        while tokio::time::Instant::now() < until {
            chunk(s, CHAT_A).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        chunk(s, b"data: [DONE]\n\n").await?;
        end(s).await?;
        return Ok(true);
    }
    // json
    let mut usage = json!({"prompt_tokens": 11, "completion_tokens": 7});
    if let Some(r) = opts.get("reasoning") {
        usage["completion_tokens_details"] = json!({"reasoning_tokens": r.parse::<i64>().unwrap()});
    }
    let echo: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    write_json(s, 200, &json!({"choices": [{"message": {"content": "hi"}}], "usage": usage, "echo": echo, "path": path})).await?;
    Ok(true)
}

// --- client ---------------------------------------------------------------------------------

pub struct Resp {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Bytes,
    pub broken: bool,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, k: &str) -> Option<String> {
        self.headers.get(k).and_then(|v| v.to_str().ok()).map(String::from)
    }
}

/// Open a request and return the response head with its body still streaming.
pub async fn open(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Vec<u8>>) -> hyper::Response<Incoming> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect proxy");
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut b = hyper::Request::builder().method(method).uri(path).header("host", "proxy");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b.body(Full::new(Bytes::from(body.unwrap_or_default()))).unwrap();
    sender.send_request(req).await.expect("send")
}

pub async fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Vec<u8>>) -> Resp {
    let resp = open(port, method, path, headers, body).await;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut inc = resp.into_body();
    let mut out = Vec::new();
    let mut broken = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), inc.frame()).await {
            Ok(Some(Ok(f))) => {
                if let Some(d) = f.data_ref() {
                    out.extend_from_slice(d);
                }
            }
            Ok(Some(Err(_))) => {
                broken = true;
                break;
            }
            Ok(None) => break,
            Err(_) => panic!("response body stalled"),
        }
    }
    Resp { status, headers, body: Bytes::from(out), broken }
}

/// POST JSON with an X-Fake-Mode.
pub async fn post(port: u16, path: &str, mode: &str, body: &Value) -> Resp {
    post_h(port, path, mode, body, &[]).await
}

pub async fn post_h(port: u16, path: &str, mode: &str, body: &Value, extra: &[(&str, &str)]) -> Resp {
    let mut h = vec![("content-type", "application/json"), ("x-fake-mode", mode)];
    h.extend_from_slice(extra);
    send(port, "POST", path, &h, Some(serde_json::to_vec(body).unwrap())).await
}

pub async fn get(port: u16, path: &str) -> Resp {
    send(port, "GET", path, &[], None).await
}

// --- config / ledger ------------------------------------------------------------------------

pub struct Opts {
    pub glm_cap: u32,
    pub qwen_cap: u32,
    pub max_depth: usize,
    /// connect, first_byte, idle, total
    pub timeouts: (f64, f64, f64, f64),
    pub eject_after: u32,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { glm_cap: 2, qwen_cap: 2, max_depth: 64, timeouts: (1.0, 5.0, 5.0, 10.0), eject_after: 2 }
    }
}

static DIRN: AtomicU64 = AtomicU64::new(0);

pub fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dgx-proxy-test-{}-{}-{}",
        std::process::id(),
        DIRN.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn config_text(ledger: &Path, backends: &[(&str, &str)], o: &Opts) -> String {
    let mut s = format!(
        r#"
listen = "127.0.0.1:0"
ledger_dir = "{ledger}"

[timeouts]
connect = {c}
first_byte = {fb}
idle = {idle}
total = {total}

[health]
interval = 60
probe_timeout = 1
eject_after = {ej}
readmit_after = 2

[queue]
max_depth = {depth}

[retry]
max_attempts = 3
backoff = [0.05, 0.05, 0.05]
"#,
        ledger = ledger.display(),
        c = o.timeouts.0,
        fb = o.timeouts.1,
        idle = o.timeouts.2,
        total = o.timeouts.3,
        ej = o.eject_after,
        depth = o.max_depth,
    );
    for (name, url) in backends {
        s += &format!("\n[[backend]]\nname = \"{name}\"\nurl = \"{url}\"\n");
    }
    s += &format!(
        r#"
[[deployment]]
name = "glm-5.3-flash-tensorfold"
served_models = ["{GLM}"]
aliases = ["glm-5.3-flash"]
cap = {glm_cap}
rewrites = ["drop-include", "function-tools-only"]

[[deployment]]
name = "qwen3.8-flash-next-single-x2"
served_models = ["{QWEN}"]
cap = {qwen_cap}

[[rule]]
name = "drop-include"
paths = ["/v1/responses"]
op = "remove"
pointer = "/include"

[[rule]]
name = "function-tools-only"
paths = ["/v1/responses", "/v1/chat/completions"]
op = "filter_array"
pointer = "/tools"
keep_where = {{ "/type" = "function" }}

[[rule]]
name = "think"
paths = ["/v1/chat/completions", "/v1/responses", "/v1/completions"]
op = "merge"
value = {{ chat_template_kwargs = {{ enable_thinking = true, thinking = true }} }}

[[rule]]
name = "nothink"
paths = ["/v1/chat/completions", "/v1/responses", "/v1/completions"]
op = "merge"
value = {{ chat_template_kwargs = {{ enable_thinking = false, thinking = false }} }}

[[profile]]
name = "think"
port = 0
rewrites = ["think"]

[[profile]]
name = "nothink"
rewrites = ["nothink"]
"#,
        glm_cap = o.glm_cap,
        qwen_cap = o.qwen_cap,
    );
    s
}

pub struct Env {
    pub proxy: dgx_proxy::Running,
    pub ledger: PathBuf,
}

impl Env {
    pub fn port(&self) -> u16 {
        self.proxy.port("main")
    }
    pub fn rows(&self) -> Vec<Value> {
        ledger_rows(&self.ledger)
    }
    pub async fn wait_rows(&self, n: usize) -> Vec<Value> {
        for _ in 0..250 {
            let r = self.rows();
            if r.len() >= n {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("ledger never reached {n} rows: {:?}", self.rows());
    }
    pub async fn last_row(&self) -> Value {
        self.rows().last().cloned().expect("a ledger row")
    }
    pub async fn probe(&self) {
        self.proxy.app.pool.probe_all().await;
    }
}

pub async fn start(backends: &[(&str, &str)], o: Opts) -> Env {
    let ledger = tmpdir();
    let cfg = dgx_proxy::Config::from_toml(&config_text(&ledger, backends, &o)).expect("config");
    let proxy = dgx_proxy::start(cfg, true).await.expect("start");
    Env { proxy, ledger }
}

pub fn ledger_rows(dir: &Path) -> Vec<Value> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return vec![],
    };
    files.sort();
    let mut rows = vec![];
    for f in files {
        for line in std::fs::read_to_string(f).unwrap_or_default().lines() {
            if let Ok(v) = serde_json::from_str(line) {
                rows.push(v);
            }
        }
    }
    rows
}

pub async fn wait_for(mut pred: impl FnMut() -> bool, secs: f64) -> bool {
    let end = tokio::time::Instant::now() + Duration::from_secs_f64(secs);
    while tokio::time::Instant::now() < end {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    pred()
}

/// name{labels} value -> (name, labels, value)
pub fn parse_metrics(text: &str) -> Vec<(String, HashMap<String, String>, f64)> {
    let mut out = vec![];
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (head, val) = line.rsplit_once(' ').unwrap();
        let (name, labels) = match head.find('{') {
            Some(i) => (&head[..i], &head[i + 1..head.len() - 1]),
            None => (head, ""),
        };
        let mut lab = HashMap::new();
        for kv in labels.split("\",").filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once("=\"").unwrap();
            lab.insert(k.to_string(), v.trim_end_matches('"').to_string());
        }
        out.push((name.to_string(), lab, val.parse().unwrap()));
    }
    out
}
