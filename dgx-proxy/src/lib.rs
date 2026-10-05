//! dgx-proxy: the one entry point for the dgx pair's inference servers.
//!
//! Routing   the request's `model` picks a deployment (served name or alias); among the backends
//!           whose `/v1/models` currently lists it, least in-flight wins (tie: least recently
//!           picked), each backend capped at the deployment's `cap`. When every backend is at its
//!           cap the request waits in a bounded FIFO here.
//! Health    GET /v1/models every `health.interval` s decides, per backend, whether it answers and
//!           which deployment it serves — a `dgx-model` switch needs no proxy restart. Connect
//!           errors / 502-504 count as failures too (passive ejection).
//! Rewrites  data-driven JSON edits per profile and per deployment (remove, filter_array, merge,
//!           set_default, rename_model); alias -> served name is automatic.
//! Retries   only before the first byte has gone to the client (connect error, 502/503/504), on
//!           another backend when one is eligible, with backoff. Never mid-stream, never on 4xx.
//! Timeouts  connect / first_byte / idle / total, by name; startup refuses total < 600 s or
//!           first_byte < 60 s (BP-60) unless told otherwise.
//! Evidence  X-Request-Id in and out, a JSONL ledger per day, usage parsed from chat JSON, chat SSE
//!           and Responses SSE (nulls stay null), Prometheus /metrics with node=<name>|fleet.

pub mod client;
pub mod config;
pub mod ledger;
pub mod metrics;
pub mod pool;
pub mod proxy;
pub mod rules;
pub mod usage;

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub use config::Config;
pub use proxy::App;

/// A started proxy. Dropping it stops the listeners and the health loop.
pub struct Running {
    pub app: Arc<App>,
    /// "main" for `listen`, plus one entry per profile that has a port.
    pub ports: HashMap<String, u16>,
    tasks: Vec<JoinHandle<()>>,
}

impl Running {
    pub fn port(&self, name: &str) -> u16 {
        self.ports[name]
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub async fn start(cfg: Config, allow_short_timeouts: bool) -> Result<Running, String> {
    cfg.validate()?;
    cfg.check_timeouts(allow_short_timeouts)?;
    let app = Arc::new(App::new(&cfg)?);
    app.pool.probe_all().await;
    let addr = cfg.listen_addr()?;
    let mut ports = HashMap::new();
    let mut tasks = vec![];
    let mut bind = |addr: SocketAddr, key: &str, profile: &str| -> Result<(), String> {
        let std_l = std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
        std_l.set_nonblocking(true).map_err(|e| e.to_string())?;
        let l = TcpListener::from_std(std_l).map_err(|e| e.to_string())?;
        ports.insert(key.to_string(), l.local_addr().map_err(|e| e.to_string())?.port());
        tasks.push(tokio::spawn(serve(l, app.clone(), Arc::from(profile))));
        Ok(())
    };
    bind(addr, "main", config::DEFAULT_PROFILE)?;
    for p in &cfg.profiles {
        if let Some(port) = p.port {
            bind(SocketAddr::new(addr.ip(), port), &p.name, &p.name)?;
        }
    }
    let a = app.clone();
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(a.health_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            a.pool.probe_all().await;
        }
    }));
    Ok(Running { app, ports, tasks })
}

async fn serve(listener: TcpListener, app: Arc<App>, profile: Arc<str>) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let (app, profile) = (app.clone(), profile.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| proxy::service(app.clone(), profile.clone(), req));
            let _ = hyper::server::conn::http1::Builder::new().timer(TokioTimer::new()).serve_connection(TokioIo::new(stream), svc).await;
        });
    }
}
