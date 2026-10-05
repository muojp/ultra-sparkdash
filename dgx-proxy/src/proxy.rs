//! The request path: local endpoints, prefixes, model routing, rewrites, retries, streaming.

use crate::client::{self, AbortOnDrop, BoxError, ConnectError, ReqBody};
use crate::config::{Config, DEFAULT_PROFILE};
use crate::ledger::{Ledger, Row};
use crate::metrics::{gauge, Metrics};
use crate::pool::{unix_now, BackendInit, DeploymentSpec, Lease, Pool, VLLM_COUNTERS, VLLM_GAUGES};
use crate::rules::{self, Rule};
use crate::usage::UsageTap;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};

pub type RespBody = BoxBody<Bytes, BoxError>;

/// TensorFold accepts up to 96 MiB (images included); leave headroom.
pub const MAX_BODY: usize = 128 << 20;

const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "host",
];

pub struct Timeouts {
    pub connect: Duration,
    pub first_byte: Duration,
    pub idle: Duration,
    pub total: Duration,
}

pub struct App {
    pub pool: Arc<Pool>,
    pub metrics: Arc<Metrics>,
    pub ledger: Ledger,
    pub rules: Vec<Rule>,
    /// profile name -> rule indices ("default" always exists).
    pub profiles: HashMap<String, Vec<usize>>,
    pub timeouts: Timeouts,
    retry_max: u32,
    backoff: Vec<f64>,
    pub health_interval: Duration,
    started: Instant,
}

enum Attempt {
    Response(Response<Incoming>, AbortOnDrop),
    FirstByteTimeout,
    Retry(String, String),
}

enum ReqBodyState {
    Full(Bytes),
    Stream(Option<Incoming>),
}

impl App {
    pub fn new(cfg: &Config) -> Result<App, String> {
        let rules: Vec<Rule> = cfg.rules.iter().map(Rule::from_cfg).collect::<Result<_, _>>()?;
        let rule_idx = |names: &[String]| -> Vec<usize> { names.iter().filter_map(|n| rules.iter().position(|r| &r.name == n)).collect() };
        let mut profiles: HashMap<String, Vec<usize>> = HashMap::new();
        profiles.insert(DEFAULT_PROFILE.to_string(), vec![]);
        for p in &cfg.profiles {
            profiles.insert(p.name.clone(), rule_idx(&p.rewrites));
        }
        let deps: Vec<DeploymentSpec> = cfg
            .deployments
            .iter()
            .map(|d| DeploymentSpec {
                name: d.name.clone(),
                served_models: d.served_models.clone(),
                aliases: d.aliases.clone(),
                cap: d.cap,
                rules: rule_idx(&d.rewrites),
            })
            .collect();
        let backends = cfg
            .backends
            .iter()
            .map(|b| Ok(BackendInit { name: b.name.clone(), url: b.url.clone(), upstream: client::Upstream::parse(&b.url)? }))
            .collect::<Result<Vec<_>, String>>()?;
        let metrics = Arc::new(Metrics::default());
        let pool = Arc::new(Pool::new(backends, deps, cfg.queue.max_depth, &cfg.health, metrics.clone()));
        let ledger = Ledger::new(cfg.ledger_path()).map_err(|e| format!("ledger_dir {}: {e}", cfg.ledger_dir))?;
        let t = &cfg.timeouts;
        Ok(App {
            pool,
            metrics,
            ledger,
            rules,
            profiles,
            timeouts: Timeouts {
                connect: Duration::from_secs_f64(t.connect),
                first_byte: Duration::from_secs_f64(t.first_byte),
                idle: Duration::from_secs_f64(t.idle),
                total: Duration::from_secs_f64(t.total),
            },
            retry_max: cfg.retry.max_attempts,
            backoff: if cfg.retry.backoff.is_empty() { vec![0.0] } else { cfg.retry.backoff.clone() },
            health_interval: Duration::from_secs_f64(cfg.health.interval),
            started: Instant::now(),
        })
    }

    /// (deployment index, the client used an alias)
    fn deployment_for(&self, model: &str) -> Option<(usize, bool)> {
        self.pool.deps.iter().enumerate().find_map(|(i, d)| {
            if d.served_models.iter().any(|m| m == model) {
                Some((i, false))
            } else if d.aliases.iter().any(|m| m == model) {
                Some((i, true))
            } else {
                None
            }
        })
    }

    fn model_ids(&self) -> Vec<String> {
        let mut out = vec![];
        for d in self.pool.eligible_deployments() {
            let spec = &self.pool.deps[d];
            out.extend(spec.served_models.iter().cloned());
            out.extend(spec.aliases.iter().cloned());
        }
        out
    }

    pub async fn handle(self: Arc<Self>, listener_profile: Arc<str>, req: Request<Incoming>) -> Response<RespBody> {
        let t0 = Instant::now();
        let rid = req
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(128).collect::<String>())
            .unwrap_or_else(gen_rid);
        let method = req.method().clone();
        let raw_path = req.uri().path().to_string();
        let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();

        if raw_path == "/health" {
            let (ok, body) = self.health();
            return json_resp(if ok { 200 } else { 503 }, &body, None);
        }
        if raw_path == "/metrics" {
            let text = self.render_metrics();
            return Response::builder()
                .status(200)
                .header("content-type", "text/plain; version=0.0.4")
                .body(full(Bytes::from(text)))
                .expect("static response");
        }
        let (prof, dispatch_id, upath) = split_prefixes(&raw_path);
        let profile = prof.unwrap_or_else(|| listener_profile.to_string());
        if upath == "/v1/models" && method == Method::GET {
            let ids = self.model_ids();
            if ids.is_empty() {
                return json_resp(503, &json!({"error": "no_upstream", "request_id": rid}), Some(&rid));
            }
            let data: Vec<Value> = ids.iter().map(|id| json!({"id": id, "object": "model", "owned_by": "dgx-proxy"})).collect();
            return json_resp(200, &json!({"object": "list", "data": data}), None);
        }

        let row = Row {
            ts: unix_now(),
            rid: rid.clone(),
            profile: profile.clone(),
            dispatch_id,
            method: method.to_string(),
            path: format!("{upath}{query}"),
            stream: None,
            deployment: None,
            model_in: None,
            model_out: None,
            rewrites: vec![],
            node: None,
            attempts: 0,
            status: None,
            outcome: None,
            queue_wait_s: 0.0,
            headers_s: None,
            ttft_s: None,
            wall_s: None,
            bytes: 0,
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            errors: vec![],
        };
        let mut rec = Recorder { app: self.clone(), row: Some(row), t0 };
        let Some(profile_rules) = self.profiles.get(&profile).cloned() else {
            return rec.local(404, "bad_route", json!({"error": "unknown_profile", "profile": profile}));
        };

        // -- request body: buffered only when routing or a rule needs it -----------------------
        let (parts, incoming) = req.into_parts();
        let has_body_method = matches!(parts.method, Method::POST | Method::PUT | Method::PATCH);
        let rule_hit = self.rules.iter().any(|r| r.applies_to(&upath));
        let mut body_state;
        let mut parsed: Option<Value> = None;
        if has_body_method && (upath.starts_with("/v1/") || rule_hit) {
            let declared =
                parts.headers.get(hyper::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            if declared.is_some_and(|n| n > MAX_BODY as u64) {
                return rec.local(413, "body_too_large", json!({"error": "body_too_large", "limit": MAX_BODY}));
            }
            let bytes = match Limited::new(incoming, MAX_BODY).collect().await {
                Ok(c) => c.to_bytes(),
                Err(e) if e.is::<LengthLimitError>() => {
                    return rec.local(413, "body_too_large", json!({"error": "body_too_large", "limit": MAX_BODY}))
                }
                Err(_) => return rec.local(400, "client_gone", json!({"error": "request_body"})),
            };
            if !bytes.is_empty() {
                parsed = serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object);
            }
            body_state = ReqBodyState::Full(bytes);
        } else {
            body_state = ReqBodyState::Stream(Some(incoming));
        }
        let model_in = parsed.as_ref().and_then(|v| v.get("model")).and_then(Value::as_str).map(String::from);
        if let Some(v) = &parsed {
            rec.row().stream = Some(v.get("stream").is_some_and(truthy));
        }
        rec.row().model_in = model_in.clone();

        // -- routing ---------------------------------------------------------------------------
        let (dep, alias) = match model_in.as_deref().and_then(|m| self.deployment_for(m)) {
            Some(x) => x,
            None => {
                let eligible = self.pool.eligible_deployments();
                match eligible.len() {
                    1 => (eligible[0], false),
                    0 => return rec.local(503, "no_upstream", json!({"error": "no_upstream"})),
                    n => {
                        return rec.local(
                            404,
                            "unknown_model",
                            json!({"error": "unknown_model",
                                   "message": format!("model {:?} is not served here and {n} deployments are up; name one", model_in.as_deref().unwrap_or("")),
                                   "models": self.model_ids()}),
                        )
                    }
                }
            }
        };
        let spec = &self.pool.deps[dep];
        rec.row().deployment = Some(spec.name.clone());

        // -- rewrites: profile rules, then deployment rules, then the alias rename -------------
        let mut applied: Vec<String> = vec![];
        if let Some(obj) = parsed.as_mut() {
            for &ri in profile_rules.iter().chain(spec.rules.iter()) {
                let r = &self.rules[ri];
                if r.applies_to(&upath) && r.apply(obj) {
                    applied.push(r.name.clone());
                }
            }
            if alias && rules::rename_model(obj, None, &self.pool.served_name(dep)) {
                applied.push("rename_model".into());
            }
            if !applied.is_empty() {
                match serde_json::to_vec(obj) {
                    Ok(b) => body_state = ReqBodyState::Full(Bytes::from(b)),
                    Err(_) => return rec.local(500, "upstream_error", json!({"error": "reserialize"})),
                }
            }
        }
        for name in &applied {
            self.metrics.inc("lb_rewrites_total", &[("rule", name), ("deployment", &spec.name), ("profile", &profile)], 1.0, false);
        }
        let model_out = parsed.as_ref().and_then(|v| v.get("model")).and_then(Value::as_str).map(String::from);
        {
            let r = rec.row();
            r.model_out = model_out;
            r.rewrites = applied;
        }

        // -- forward ---------------------------------------------------------------------------
        let fwd_headers = forward_headers(&parts.headers, &rid);
        let path_q = format!("{upath}{query}");
        let fb_deadline = t0 + self.timeouts.first_byte.min(self.timeouts.total);
        let total_deadline = t0 + self.timeouts.total;
        let mut avoid: Vec<usize> = vec![];
        for attempt in 1..=self.retry_max {
            rec.row().attempts = attempt;
            let qt = Instant::now();
            let lease = self.pool.acquire(dep, &avoid, fb_deadline).await;
            rec.row().queue_wait_s = r3(rec.row().queue_wait_s + qt.elapsed().as_secs_f64());
            let lease = match lease {
                Ok(l) => l,
                Err(why) => {
                    let code = if why == "timeout_first_byte" { 504 } else { 503 };
                    return rec.local(code, why, json!({"error": why}));
                }
            };
            let node = self.pool.backend_name(lease.idx);
            rec.row().node = Some(node.clone());
            let body: ReqBody = match &mut body_state {
                ReqBodyState::Full(b) => full(b.clone()),
                ReqBodyState::Stream(s) => match s.take() {
                    Some(inc) => inc.map_err(|e| Box::new(e) as BoxError).boxed(),
                    None => break, // a streamed body cannot be replayed
                },
            };
            match self.attempt(lease.idx, &parts.method, &path_q, &fwd_headers, body, fb_deadline).await {
                Attempt::Response(resp, conn) => {
                    return self.stream_response(rec, lease, resp, conn, total_deadline);
                }
                Attempt::FirstByteTimeout => {
                    rec.row().errors.push(format!("{node}: first_byte timeout"));
                    self.pool.passive_failure(lease.idx, dep, "timeout_first_byte");
                    return rec.local(504, "timeout_first_byte", json!({"error": "timeout_first_byte"}));
                }
                Attempt::Retry(err, kind) => {
                    rec.row().errors.push(format!("{node}: {err}"));
                    self.pool.passive_failure(lease.idx, dep, &kind);
                    avoid.push(lease.idx);
                    drop(lease);
                    self.metrics.inc(
                        "lb_retries_total",
                        &[("served_model", spec.label()), ("deployment", &spec.name), ("node", &node), ("profile", &profile)],
                        1.0,
                        true,
                    );
                    if attempt < self.retry_max {
                        let pause = self.backoff[(attempt as usize - 1).min(self.backoff.len() - 1)];
                        let pause = Duration::from_secs_f64(pause.max(0.0));
                        if Instant::now() + pause >= fb_deadline {
                            break;
                        }
                        tokio::time::sleep(pause).await;
                    }
                }
            }
        }
        let detail: Vec<String> = {
            let e = &rec.row().errors;
            e[e.len().saturating_sub(3)..].to_vec()
        };
        rec.local(502, "upstream_error", json!({"error": "upstream_error", "detail": detail}))
    }

    async fn attempt(
        &self,
        idx: usize,
        method: &Method,
        path_q: &str,
        headers: &HeaderMap,
        body: ReqBody,
        fb_deadline: Instant,
    ) -> Attempt {
        let up = self.pool.upstream(idx);
        let now = Instant::now();
        if now >= fb_deadline {
            return Attempt::FirstByteTimeout;
        }
        let ct = self.timeouts.connect.min(fb_deadline - now);
        let mut conn = match client::connect(up, ct).await {
            Ok(c) => c,
            Err(ConnectError::Timeout) => {
                if Instant::now() + Duration::from_millis(10) >= fb_deadline {
                    return Attempt::FirstByteTimeout;
                }
                return Attempt::Retry("connect timeout".into(), "connect".into());
            }
            Err(ConnectError::Io(e)) => return Attempt::Retry(e, "connect".into()),
        };
        let mut builder = Request::builder().method(method.clone()).uri(format!("{}{}", up.prefix, path_q));
        if let Some(h) = builder.headers_mut() {
            h.extend(headers.iter().map(|(k, v)| (k.clone(), v.clone())));
            if let Ok(v) = HeaderValue::from_str(&up.authority) {
                h.insert(hyper::header::HOST, v);
            }
        }
        let req = match builder.body(body) {
            Ok(r) => r,
            Err(e) => return Attempt::Retry(format!("build: {e}"), "connect".into()),
        };
        match tokio::time::timeout_at(fb_deadline, conn.sender.send_request(req)).await {
            Err(_) => Attempt::FirstByteTimeout,
            Ok(Err(e)) => Attempt::Retry(format!("request: {e}"), "connect".into()),
            Ok(Ok(resp)) => {
                let s = resp.status().as_u16();
                if (502..=504).contains(&s) {
                    Attempt::Retry(format!("HTTP {s}"), format!("http_{s}"))
                } else {
                    Attempt::Response(resp, conn.task)
                }
            }
        }
    }

    /// From here the client gets this response, whatever happens: no more retries.
    fn stream_response(
        self: &Arc<Self>,
        mut rec: Recorder,
        lease: Lease,
        resp: Response<Incoming>,
        conn: AbortOnDrop,
        total_deadline: Instant,
    ) -> Response<RespBody> {
        self.pool.passive_success(lease.idx);
        let (parts, inner) = resp.into_parts();
        let ct = parts.headers.get(hyper::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let clen = parts.headers.get(hyper::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        let mut row = rec.row.take().expect("row");
        row.status = Some(parts.status.as_u16());
        row.headers_s = Some(r3(rec.t0.elapsed().as_secs_f64()));
        let mut out = Response::builder().status(parts.status);
        if let Some(h) = out.headers_mut() {
            for (k, v) in parts.headers.iter() {
                if !HOP_BY_HOP.contains(&k.as_str()) {
                    h.append(k.clone(), v.clone());
                }
            }
            if let Ok(v) = HeaderValue::from_str(&row.rid) {
                h.insert("x-request-id", v);
            }
            if let Some(n) = &row.node {
                if let Ok(v) = HeaderValue::from_str(n) {
                    h.insert("x-dgx-lb-node", v);
                }
            }
        }
        let spec = &self.pool.deps[lease.dep];
        let now = Instant::now();
        let body = ProxyBody {
            inner,
            idle: self.timeouts.idle,
            idle_sleep: Box::pin(tokio::time::sleep_until(now + self.timeouts.idle)),
            total_sleep: Box::pin(tokio::time::sleep_until(total_deadline)),
            clen,
            fin: Some(Finish {
                app: self.clone(),
                node: row.node.clone().unwrap_or_default(),
                served: spec.label().to_string(),
                deployment: spec.name.clone(),
                row,
                tap: UsageTap::new(&ct),
                t0: rec.t0,
                lease: Some(lease),
            }),
            _conn: conn,
        };
        out.body(body.boxed()).unwrap_or_else(|_| json_resp(500, &json!({"error": "response build"}), None))
    }

    /// Metrics + ledger for a finished request.
    fn finish_row(&self, row: &Row) {
        let dep = row.deployment.as_ref().and_then(|n| self.pool.deps.iter().find(|d| &d.name == n));
        let served = dep.map(|d| d.label()).unwrap_or("none");
        let dname = row.deployment.as_deref().unwrap_or("none");
        let node = row.node.as_deref().unwrap_or("none");
        let outcome = row.outcome.as_deref().unwrap_or("unknown");
        let base = [("served_model", served), ("deployment", dname), ("node", node), ("profile", row.profile.as_str())];
        let mut with_outcome = base.to_vec();
        with_outcome.push(("outcome", outcome));
        self.metrics.inc("lb_requests_total", &with_outcome, 1.0, true);
        for (v, name) in [
            (row.prompt_tokens, "lb_prompt_tokens_total"),
            (row.completion_tokens, "lb_generation_tokens_total"),
            (row.reasoning_tokens, "lb_reasoning_tokens_total"),
        ] {
            if let Some(v) = v {
                self.metrics.inc(name, &base, v as f64, true);
            }
        }
        self.ledger.write(row);
    }

    pub fn health(&self) -> (bool, Value) {
        let (bs, depth) = self.pool.snapshot();
        let serving = self.pool.serving(&bs);
        let mut deps = serde_json::Map::new();
        let mut served = vec![];
        let mut eligible = 0;
        for (d, spec) in self.pool.deps.iter().enumerate() {
            let names: Vec<&str> = serving[d].iter().map(|&i| bs[i].name.as_str()).collect();
            if !names.is_empty() {
                eligible += 1;
                served.extend(spec.served_models.iter().cloned());
            }
            deps.insert(
                spec.name.clone(),
                json!({
                    "served_models": spec.served_models, "aliases": spec.aliases, "cap": spec.cap,
                    "eligible": !names.is_empty(), "backends": names,
                    "inflight": bs.iter().map(|b| b.inflight_by_dep[d]).sum::<u32>(),
                    "queue_depth": depth[d],
                    "rewrites": spec.rules.iter().map(|&r| self.rules[r].name.clone()).collect::<Vec<_>>(),
                }),
            );
        }
        let mut backends = serde_json::Map::new();
        for (i, b) in bs.iter().enumerate() {
            let ds: Vec<&str> =
                (0..self.pool.deps.len()).filter(|d| serving[*d].contains(&i)).map(|d| self.pool.deps[d].name.as_str()).collect();
            backends.insert(
                b.name.clone(),
                json!({"url": b.url, "up": b.up, "fails": b.fails, "inflight": b.inflight, "models": b.models,
                       "deployments": ds, "last_probe": b.last_probe}),
            );
        }
        let body = json!({
            "eligible": eligible,
            "served_models": served,
            "deployments": deps,
            "backends": backends,
            "queue_depth": depth.iter().sum::<usize>(),
            "uptime_s": r3(self.started.elapsed().as_secs_f64()),
        });
        (eligible > 0, body)
    }

    pub fn render_metrics(&self) -> String {
        let mut out = String::new();
        self.metrics.render_into(&mut out);
        let (bs, depth) = self.pool.snapshot();
        let serving = self.pool.serving(&bs);
        type Rows = Vec<(Vec<(String, String)>, f64)>;
        let (mut up, mut inflight, mut cap, mut queue): (Rows, Rows, Rows, Rows) = Default::default();
        let mut vg: Vec<Rows> = vec![vec![]; VLLM_GAUGES.len()];
        let mut vc: Vec<Rows> = vec![vec![]; VLLM_COUNTERS.len()];
        for (d, spec) in self.pool.deps.iter().enumerate() {
            let lab = |node: &str| {
                vec![
                    ("served_model".to_string(), spec.label().to_string()),
                    ("deployment".to_string(), spec.name.clone()),
                    ("node".to_string(), node.to_string()),
                ]
            };
            let (mut f_up, mut f_inf, mut f_cap) = (0.0, 0.0, 0.0);
            for (i, b) in bs.iter().enumerate() {
                let s = serving[d].contains(&i);
                up.push((lab(&b.name), if s { 1.0 } else { 0.0 }));
                inflight.push((lab(&b.name), b.inflight_by_dep[d] as f64));
                cap.push((lab(&b.name), spec.cap as f64));
                f_inf += b.inflight_by_dep[d] as f64;
                if s {
                    f_up += 1.0;
                    f_cap += spec.cap as f64;
                }
            }
            up.push((lab("fleet"), f_up));
            inflight.push((lab("fleet"), f_inf));
            cap.push((lab("fleet"), f_cap));
            queue.push((lab("fleet"), depth[d] as f64));
            for (k, key) in VLLM_GAUGES.iter().enumerate() {
                let mut sum = 0.0;
                for &i in &serving[d] {
                    if let Some(Some(v)) = bs[i].vllm.iter().find(|x| x.0 == *key).map(|x| x.1) {
                        vg[k].push((lab(&bs[i].name), v));
                        sum += v;
                    }
                }
                vg[k].push((lab("fleet"), sum));
            }
            // per node only: a replica restart resets them, so no fleet sum here
            for (k, key) in VLLM_COUNTERS.iter().enumerate() {
                for &i in &serving[d] {
                    if let Some(Some(v)) = bs[i].vllm.iter().find(|x| x.0 == *key).map(|x| x.1) {
                        vc[k].push((lab(&bs[i].name), v));
                    }
                }
            }
        }
        gauge(&mut out, "lb_node_up", "gauge", &up);
        gauge(&mut out, "lb_requests_inflight", "gauge", &inflight);
        gauge(&mut out, "lb_node_cap", "gauge", &cap);
        gauge(&mut out, "lb_queue_depth", "gauge", &queue);
        for (k, key) in VLLM_GAUGES.iter().enumerate() {
            gauge(&mut out, &format!("lb_vllm_{key}"), "gauge", &vg[k]);
        }
        for (k, key) in VLLM_COUNTERS.iter().enumerate() {
            gauge(&mut out, &format!("lb_vllm_{key}"), "counter", &vc[k]);
        }
        out
    }
}

/// Owns the ledger row until the request ends. Dropped unfinished (the client went away while
/// the request was queued or waiting for headers) it records `client_gone`.
struct Recorder {
    app: Arc<App>,
    row: Option<Row>,
    t0: Instant,
}

impl Recorder {
    fn row(&mut self) -> &mut Row {
        self.row.as_mut().expect("row taken")
    }

    fn local(mut self, code: u16, outcome: &str, mut body: Value) -> Response<RespBody> {
        let mut row = self.row.take().expect("row");
        row.status = Some(code);
        row.outcome = Some(outcome.to_string());
        row.wall_s = Some(r3(self.t0.elapsed().as_secs_f64()));
        let rid = row.rid.clone();
        if let Some(o) = body.as_object_mut() {
            o.insert("request_id".into(), Value::String(rid.clone()));
        }
        self.app.finish_row(&row);
        json_resp(code, &body, Some(&rid))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(mut row) = self.row.take() {
            row.outcome = Some("client_gone".into());
            row.wall_s = Some(r3(self.t0.elapsed().as_secs_f64()));
            self.app.finish_row(&row);
        }
    }
}

struct Finish {
    app: Arc<App>,
    row: Row,
    tap: UsageTap,
    t0: Instant,
    lease: Option<Lease>,
    node: String,
    served: String,
    deployment: String,
}

impl Finish {
    fn on_data(&mut self, d: &Bytes) {
        if self.row.ttft_s.is_none() {
            // first body bytes: the closest proxy-side TTFT (vLLM sends SSE headers at once)
            let ttft = self.t0.elapsed().as_secs_f64();
            self.row.ttft_s = Some(r3(ttft));
            self.app.metrics.observe(
                "lb_ttft_seconds",
                &[("served_model", &self.served), ("deployment", &self.deployment), ("node", &self.node)],
                ttft,
            );
        }
        self.tap.feed(d);
        self.row.bytes += d.len() as u64;
    }

    fn done(mut self, outcome: &str) {
        self.lease.take(); // free the slot before the bookkeeping
        let u = self.tap.finish();
        self.row.prompt_tokens = u.prompt_tokens;
        self.row.completion_tokens = u.completion_tokens;
        self.row.reasoning_tokens = u.reasoning_tokens;
        self.row.outcome = Some(outcome.to_string());
        self.row.wall_s = Some(r3(self.t0.elapsed().as_secs_f64()));
        self.app.finish_row(&self.row);
    }
}

/// Streams the upstream body frame by frame (no buffering), with the idle and total timers, a
/// usage tap, and the ledger row written when the stream ends or the client drops it.
struct ProxyBody {
    inner: Incoming,
    idle: Duration,
    idle_sleep: Pin<Box<Sleep>>,
    total_sleep: Pin<Box<Sleep>>,
    clen: Option<u64>,
    fin: Option<Finish>,
    _conn: AbortOnDrop,
}

impl ProxyBody {
    fn end(&mut self, outcome: &str) {
        if let Some(f) = self.fin.take() {
            f.done(outcome);
        }
    }

    fn ok_outcome(&self) -> &'static str {
        let f = self.fin.as_ref().expect("fin");
        if self.clen.is_some_and(|n| f.row.bytes < n) {
            "midstream_broken"
        } else if f.row.attempts <= 1 {
            "ok"
        } else {
            "retried_ok"
        }
    }
}

impl Body for ProxyBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if this.fin.is_none() {
            return Poll::Ready(None);
        }
        if this.total_sleep.as_mut().poll(cx).is_ready() {
            this.end("timeout_total");
            return Poll::Ready(Some(Err("timeout_total".into())));
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(d) = frame.data_ref() {
                    if !d.is_empty() {
                        this.fin.as_mut().expect("fin").on_data(d);
                    }
                }
                let next = Instant::now() + this.idle;
                this.idle_sleep.as_mut().reset(next);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                let f = this.fin.as_mut().expect("fin");
                f.row.errors.push(format!("{}: midstream {e}", f.node));
                this.end("midstream_broken");
                Poll::Ready(Some(Err(Box::new(e))))
            }
            Poll::Ready(None) => {
                let o = this.ok_outcome();
                this.end(o);
                Poll::Ready(None)
            }
            Poll::Pending => {
                if this.idle_sleep.as_mut().poll(cx).is_ready() {
                    this.end("timeout_idle");
                    Poll::Ready(Some(Err("timeout_idle".into())))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.clen {
            Some(n) => SizeHint::with_exact(n),
            None => SizeHint::default(),
        }
    }
}

impl Drop for ProxyBody {
    fn drop(&mut self) {
        if let Some(f) = &self.fin {
            // Every declared byte went out (hyper may skip the final poll of a sized body).
            let outcome = if self.clen.is_some_and(|n| f.row.bytes >= n) { self.ok_outcome() } else { "client_gone" };
            self.end(outcome);
        }
    }
}

// --- helpers ----------------------------------------------------------------------------------

/// `/p/<profile>` and `/d/<dispatch_id>` prefixes, in either order, each at most once.
pub fn split_prefixes(path: &str) -> (Option<String>, Option<String>, String) {
    let mut rest = path;
    let (mut prof, mut disp) = (None, None);
    loop {
        if prof.is_none() {
            if let Some((seg, tail)) = rest.strip_prefix("/p/").and_then(split_seg) {
                prof = Some(seg.to_string());
                rest = tail;
                continue;
            }
        }
        if disp.is_none() {
            if let Some((seg, tail)) = rest.strip_prefix("/d/").and_then(split_seg) {
                if seg.len() <= 128 && seg.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c)) {
                    disp = Some(seg.to_string());
                    rest = tail;
                    continue;
                }
            }
        }
        break;
    }
    (prof, disp, rest.to_string())
}

fn split_seg(s: &str) -> Option<(&str, &str)> {
    let i = s.find('/')?;
    (i > 0).then(|| (&s[..i], &s[i..]))
}

fn forward_headers(src: &HeaderMap, rid: &str) -> HeaderMap {
    let conn_listed: Vec<String> = src
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|s| s.trim().to_ascii_lowercase()))
        .collect();
    let mut h = HeaderMap::new();
    for (k, v) in src.iter() {
        let name = k.as_str();
        if HOP_BY_HOP.contains(&name) || conn_listed.iter().any(|c| c == name) || name == "x-request-id" {
            continue;
        }
        h.append(k.clone(), v.clone());
    }
    if let Ok(v) = HeaderValue::from_str(rid) {
        h.insert(HeaderName::from_static("x-request-id"), v);
    }
    h
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn full(b: Bytes) -> BoxBody<Bytes, BoxError> {
    Full::new(b).map_err(|never| match never {}).boxed()
}

fn json_resp(code: u16, body: &Value, rid: Option<&str>) -> Response<RespBody> {
    let mut b = Response::builder()
        .status(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json");
    if let Some(r) = rid {
        if let Ok(v) = HeaderValue::from_str(r) {
            b = b.header("x-request-id", v);
        }
    }
    b.body(full(Bytes::from(serde_json::to_vec(body).unwrap_or_default()))).expect("static response")
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn gen_rid() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    h.write_u128(nanos);
    h.write_u64(CTR.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", h.finish())
}

/// hyper service entry point.
pub async fn service(app: Arc<App>, profile: Arc<str>, req: Request<Incoming>) -> Result<Response<RespBody>, Infallible> {
    Ok(app.handle(profile, req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(split_prefixes("/v1/x"), (None, None, "/v1/x".into()));
        assert_eq!(split_prefixes("/p/think/v1/x"), (Some("think".into()), None, "/v1/x".into()));
        assert_eq!(split_prefixes("/d/bb2-abc/p/n/v1/x"), (Some("n".into()), Some("bb2-abc".into()), "/v1/x".into()));
        assert_eq!(split_prefixes("/p/n/d/a.b:c/v1/x"), (Some("n".into()), Some("a.b:c".into()), "/v1/x".into()));
        assert_eq!(split_prefixes("/d/bad id/v1"), (None, None, "/d/bad id/v1".into()));
        assert_eq!(gen_rid().len(), 16);
    }
}
