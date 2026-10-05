//! Backends, health and the scheduler. One pool for every listener, so in-flight counts per
//! backend are global across profiles and ports (two balancers would each think a node is free).
//!
//! Which deployment a backend serves is not configured: the health probe's `/v1/models` answer
//! decides it, so a `dgx-model` switch needs no proxy restart.

use crate::client::{self, Upstream};
use crate::metrics::Metrics;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct DeploymentSpec {
    pub name: String,
    pub served_models: Vec<String>,
    pub aliases: Vec<String>,
    pub cap: u32,
    /// Indices into the rule table.
    pub rules: Vec<usize>,
}

impl DeploymentSpec {
    /// The `served_model` metric label.
    pub fn label(&self) -> &str {
        &self.served_models[0]
    }
}

#[derive(Debug, Clone)]
pub struct BackendState {
    pub name: String,
    pub url: String,
    pub up: bool,
    pub fails: u32,
    pub goods: u32,
    pub models: Vec<String>,
    pub last_probe: Option<Value>,
    pub inflight: u32,
    pub inflight_by_dep: Vec<u32>,
    pub last_pick: u64,
    /// Last scraped vllm:* values (None = not reported).
    pub vllm: Vec<(&'static str, Option<f64>)>,
}

struct Waiter {
    id: u64,
    dep: usize,
    avoid: Vec<usize>,
    tx: oneshot::Sender<Result<usize, &'static str>>,
}

struct State {
    backends: Vec<BackendState>,
    queue: VecDeque<Waiter>,
    next_id: u64,
    pick_seq: u64,
}

pub struct Pool {
    pub deps: Vec<DeploymentSpec>,
    upstreams: Vec<Upstream>,
    state: Mutex<State>,
    max_depth: usize,
    eject_after: u32,
    readmit_after: u32,
    probe_timeout: Duration,
    metrics: Arc<Metrics>,
}

enum Pick {
    Got(usize),
    Full,
    NoUpstream,
}

pub const VLLM_GAUGES: [&str; 3] = ["running", "waiting", "waiting_capacity"];
pub const VLLM_COUNTERS: [&str; 2] = ["generation_tokens_total", "prompt_tokens_total"];

pub struct BackendInit {
    pub name: String,
    pub url: String,
    pub upstream: Upstream,
}

impl Pool {
    pub fn new(
        backends: Vec<BackendInit>,
        deps: Vec<DeploymentSpec>,
        max_depth: usize,
        health: &crate::config::Health,
        metrics: Arc<Metrics>,
    ) -> Pool {
        let n = deps.len();
        let states = backends
            .iter()
            .map(|b| BackendState {
                name: b.name.clone(),
                url: b.url.clone(),
                up: false,
                fails: 0,
                goods: 0,
                models: vec![],
                last_probe: None,
                inflight: 0,
                inflight_by_dep: vec![0; n],
                last_pick: 0,
                vllm: vec![],
            })
            .collect();
        Pool {
            deps,
            upstreams: backends.into_iter().map(|b| b.upstream).collect(),
            state: Mutex::new(State { backends: states, queue: VecDeque::new(), next_id: 0, pick_seq: 0 }),
            max_depth,
            eject_after: health.eject_after.max(1),
            readmit_after: health.readmit_after.max(1),
            probe_timeout: Duration::from_secs_f64(health.probe_timeout),
            metrics,
        }
    }

    pub fn upstream(&self, idx: usize) -> &Upstream {
        &self.upstreams[idx]
    }

    pub fn backend_name(&self, idx: usize) -> String {
        self.state.lock().unwrap().backends[idx].name.clone()
    }

    fn serves(&self, b: &BackendState, dep: usize) -> bool {
        b.up && b.models.iter().any(|m| self.deps[dep].served_models.contains(m))
    }

    /// Snapshot for /health, /metrics and tests.
    pub fn snapshot(&self) -> (Vec<BackendState>, Vec<usize>) {
        let s = self.state.lock().unwrap();
        let mut depth = vec![0; self.deps.len()];
        for w in &s.queue {
            depth[w.dep] += 1;
        }
        (s.backends.clone(), depth)
    }

    /// Backends currently serving each deployment.
    pub fn serving(&self, backends: &[BackendState]) -> Vec<Vec<usize>> {
        (0..self.deps.len()).map(|d| (0..backends.len()).filter(|&i| self.serves(&backends[i], d)).collect()).collect()
    }

    pub fn eligible_deployments(&self) -> Vec<usize> {
        let s = self.state.lock().unwrap();
        (0..self.deps.len()).filter(|&d| s.backends.iter().any(|b| self.serves(b, d))).collect()
    }

    /// First served name of `dep` that an eligible backend reports (fallback: the first configured).
    pub fn served_name(&self, dep: usize) -> String {
        let s = self.state.lock().unwrap();
        let d = &self.deps[dep];
        for m in &d.served_models {
            if s.backends.iter().any(|b| b.up && b.models.contains(m)) {
                return m.clone();
            }
        }
        d.served_models[0].clone()
    }

    pub fn queue_len(&self) -> usize {
        self.state.lock().unwrap().queue.len()
    }

    // --- scheduling --------------------------------------------------------------------------

    fn try_pick(&self, s: &mut State, dep: usize, avoid: &[usize]) -> Pick {
        let cap = self.deps[dep].cap;
        let eligible: Vec<usize> = (0..s.backends.len()).filter(|&i| self.serves(&s.backends[i], dep)).collect();
        if eligible.is_empty() {
            return Pick::NoUpstream;
        }
        let preferred: Vec<usize> = eligible.iter().copied().filter(|i| !avoid.contains(i)).collect();
        let preferred = if preferred.is_empty() { eligible } else { preferred };
        let best = preferred
            .into_iter()
            .filter(|&i| s.backends[i].inflight < cap)
            .min_by_key(|&i| (s.backends[i].inflight, s.backends[i].last_pick));
        match best {
            None => Pick::Full,
            Some(i) => {
                s.pick_seq += 1;
                let b = &mut s.backends[i];
                b.inflight += 1;
                b.inflight_by_dep[dep] += 1;
                b.last_pick = s.pick_seq;
                Pick::Got(i)
            }
        }
    }

    fn unpick(s: &mut State, idx: usize, dep: usize) {
        let b = &mut s.backends[idx];
        b.inflight = b.inflight.saturating_sub(1);
        b.inflight_by_dep[dep] = b.inflight_by_dep[dep].saturating_sub(1);
    }

    /// Hand free slots to waiters in FIFO order. A waiter that cannot be served blocks the later
    /// waiters of its own deployment only; a deployment with no backend left fails its waiters.
    fn dispatch(&self, s: &mut State) {
        let mut blocked = vec![false; self.deps.len()];
        let mut i = 0;
        while i < s.queue.len() {
            let (dep, avoid) = (s.queue[i].dep, s.queue[i].avoid.clone());
            if blocked[dep] {
                i += 1;
                continue;
            }
            match self.try_pick(s, dep, &avoid) {
                Pick::Full => {
                    blocked[dep] = true;
                    i += 1;
                }
                Pick::Got(idx) => {
                    let w = s.queue.remove(i).expect("index in range");
                    if w.tx.send(Ok(idx)).is_err() {
                        Self::unpick(s, idx, dep);
                    }
                }
                Pick::NoUpstream => {
                    let w = s.queue.remove(i).expect("index in range");
                    let _ = w.tx.send(Err("no_upstream"));
                }
            }
        }
    }

    /// Wait for a slot on a backend serving `dep`. Err is an outcome: no_upstream, queue_full or
    /// timeout_first_byte (the deadline passed while queued).
    pub async fn acquire(self: &Arc<Self>, dep: usize, avoid: &[usize], deadline: Instant) -> Result<Lease, &'static str> {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut s = self.state.lock().unwrap();
            let others_waiting = s.queue.iter().any(|w| w.dep == dep);
            if !others_waiting {
                match self.try_pick(&mut s, dep, avoid) {
                    Pick::Got(idx) => return Ok(Lease { pool: self.clone(), idx, dep }),
                    Pick::NoUpstream => return Err("no_upstream"),
                    Pick::Full => {}
                }
            }
            if s.queue.len() >= self.max_depth {
                return Err("queue_full");
            }
            s.next_id += 1;
            let id = s.next_id;
            s.queue.push_back(Waiter { id, dep, avoid: avoid.to_vec(), tx });
            self.dispatch(&mut s);
            id
        };
        let mut guard = WaitGuard { pool: self.clone(), id, dep, rx: Some(rx) };
        let rx = guard.rx.as_mut().unwrap();
        let res = match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("no_upstream"),
            Err(_) => {
                // Deadline: leave the queue, unless a slot was handed over at the same moment.
                let mut s = self.state.lock().unwrap();
                if let Some(pos) = s.queue.iter().position(|w| w.id == id) {
                    s.queue.remove(pos);
                    Err("timeout_first_byte")
                } else {
                    drop(s);
                    match guard.rx.as_mut().unwrap().try_recv() {
                        Ok(r) => r,
                        Err(_) => Err("timeout_first_byte"),
                    }
                }
            }
        };
        guard.rx = None;
        res.map(|idx| Lease { pool: self.clone(), idx, dep })
    }

    fn release(&self, idx: usize, dep: usize) {
        let mut s = self.state.lock().unwrap();
        Self::unpick(&mut s, idx, dep);
        self.dispatch(&mut s);
    }

    // --- health ------------------------------------------------------------------------------

    fn mark(&self, s: &mut State, idx: usize, ok: bool) {
        let b = &mut s.backends[idx];
        if ok {
            b.fails = 0;
            if !b.up {
                b.goods += 1;
                if b.goods >= self.readmit_after {
                    b.up = true;
                    b.goods = 0;
                }
            }
        } else {
            b.goods = 0;
            b.fails += 1;
            if b.fails >= self.eject_after {
                b.up = false;
            }
        }
    }

    pub fn passive_success(&self, idx: usize) {
        self.state.lock().unwrap().backends[idx].fails = 0;
    }

    pub fn passive_failure(&self, idx: usize, dep: usize, kind: &str) {
        let name = self.backend_name(idx);
        let d = &self.deps[dep];
        self.metrics.inc(
            "lb_upstream_errors_total",
            &[("served_model", d.label()), ("deployment", &d.name), ("node", &name), ("kind", kind)],
            1.0,
            true,
        );
        let mut s = self.state.lock().unwrap();
        self.mark(&mut s, idx, false);
        self.dispatch(&mut s);
    }

    pub async fn probe(&self, idx: usize) {
        let up = &self.upstreams[idx];
        let res = client::get(up, "/v1/models", self.probe_timeout, 8 << 20).await;
        let (ok, reason, models): (bool, String, Option<Vec<String>>) = match res {
            Ok((200, body)) => match serde_json::from_slice::<Value>(&body) {
                Ok(v) => {
                    let ids = v
                        .get("data")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from)).collect());
                    match ids {
                        Some(ids) => (true, "ok".into(), Some(ids)),
                        None => (false, "bad_models_json".into(), None),
                    }
                }
                Err(_) => (false, "bad_models_json".into(), None),
            },
            Ok((code, _)) => (false, format!("http_{code}"), None),
            Err(e) => (false, e, None),
        };
        {
            let mut s = self.state.lock().unwrap();
            let first = s.backends[idx].last_probe.is_none();
            s.backends[idx].last_probe = Some(json!({"ts": unix_now(), "ok": ok, "reason": reason, "models": models}));
            if let Some(m) = models {
                s.backends[idx].models = m;
            }
            if ok && first {
                let b = &mut s.backends[idx];
                b.up = true;
                b.fails = 0;
                b.goods = 0;
            } else {
                self.mark(&mut s, idx, ok);
            }
            if !ok {
                s.backends[idx].vllm.clear();
            }
            self.dispatch(&mut s);
        }
        if ok {
            let vllm = match client::get(up, "/metrics", self.probe_timeout, 16 << 20).await {
                Ok((200, body)) => parse_vllm(&String::from_utf8_lossy(&body)),
                _ => vec![],
            };
            self.state.lock().unwrap().backends[idx].vllm = vllm;
        }
    }

    pub async fn probe_all(self: &Arc<Self>) {
        let mut set = tokio::task::JoinSet::new();
        for i in 0..self.upstreams.len() {
            let p = self.clone();
            set.spawn(async move { p.probe(i).await });
        }
        while set.join_next().await.is_some() {}
    }
}

/// A slot on a backend. Dropping it releases the slot and wakes the queue.
pub struct Lease {
    pool: Arc<Pool>,
    pub idx: usize,
    pub dep: usize,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.release(self.idx, self.dep);
    }
}

/// Removes a cancelled waiter from the queue (client gone while queued), and gives back a slot
/// that was handed over but never taken.
struct WaitGuard {
    pool: Arc<Pool>,
    id: u64,
    dep: usize,
    rx: Option<oneshot::Receiver<Result<usize, &'static str>>>,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else { return };
        let mut s = self.pool.state.lock().unwrap();
        if let Some(pos) = s.queue.iter().position(|w| w.id == self.id) {
            s.queue.remove(pos);
            return;
        }
        if let Ok(Ok(idx)) = rx.try_recv() {
            Pool::unpick(&mut s, idx, self.dep);
            self.pool.dispatch(&mut s);
        }
    }
}

pub fn unix_now() -> f64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    (t * 1000.0).round() / 1000.0
}

/// vllm:* lines from a backend's Prometheus text, summed over label sets.
pub fn parse_vllm(text: &str) -> Vec<(&'static str, Option<f64>)> {
    let wanted: [(&'static str, &str, Option<&str>); 5] = [
        ("running", "vllm:num_requests_running", None),
        ("waiting", "vllm:num_requests_waiting", None),
        ("waiting_capacity", "vllm:num_requests_waiting_by_reason", Some("reason=\"capacity\"")),
        ("generation_tokens_total", "vllm:generation_tokens_total", None),
        ("prompt_tokens_total", "vllm:prompt_tokens_total", None),
    ];
    let mut out: Vec<(&'static str, Option<f64>)> = wanted.iter().map(|w| (w.0, None)).collect();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some(brace) = line.find('{') else { continue };
        let name = &line[..brace];
        let Some(close) = line[brace..].find('}') else { continue };
        let labels = &line[brace + 1..brace + close];
        let Some(val) = line[brace + close + 1..].split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        for (i, (_, metric, need)) in wanted.iter().enumerate() {
            if name == *metric && need.is_none_or(|n| labels.contains(n)) {
                *out[i].1.get_or_insert(0.0) += val;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vllm_parse() {
        let t = "# HELP x\nvllm:num_requests_running{model_name=\"m\"} 3\nvllm:num_requests_waiting{model_name=\"m\"} 1\n\
                 vllm:num_requests_waiting_by_reason{model_name=\"m\",reason=\"capacity\"} 2\n\
                 vllm:num_requests_waiting_by_reason{model_name=\"m\",reason=\"other\"} 5\n\
                 vllm:generation_tokens_total{model_name=\"m\"} 10\n";
        let v = parse_vllm(t);
        let get = |k: &str| v.iter().find(|x| x.0 == k).unwrap().1;
        assert_eq!(get("running"), Some(3.0));
        assert_eq!(get("waiting_capacity"), Some(2.0));
        assert_eq!(get("generation_tokens_total"), Some(10.0));
        assert_eq!(get("prompt_tokens_total"), None);
    }
}
