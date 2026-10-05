//! Config file (TOML). `config.example.toml` documents every key.

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;

/// BP-60: a 20 s "budget" once turned 14 healthy calls into 502s. Startup refuses anything shorter
/// unless the operator passes `--i-know-short-timeouts` (tests do).
pub const MIN_TOTAL_S: f64 = 600.0;
pub const MIN_FIRST_BYTE_S: f64 = 60.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_ledger_dir")]
    pub ledger_dir: String,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub queue: Queue,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendCfg>,
    #[serde(default, rename = "deployment")]
    pub deployments: Vec<DeploymentCfg>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<RuleCfg>,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<ProfileCfg>,
}

fn default_listen() -> String {
    "0.0.0.0:8880".into()
}
fn default_ledger_dir() -> String {
    "~/dgx-proxy/ledger".into()
}

/// Seconds. `first_byte` covers queueing + connect + response headers; `idle` is the longest gap
/// between two body frames; `total` bounds the whole request.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub connect: f64,
    pub first_byte: f64,
    pub idle: f64,
    pub total: f64,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self { connect: 5.0, first_byte: 900.0, idle: 300.0, total: 3900.0 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Health {
    pub interval: f64,
    pub probe_timeout: f64,
    pub eject_after: u32,
    pub readmit_after: u32,
}
impl Default for Health {
    fn default() -> Self {
        Self { interval: 5.0, probe_timeout: 3.0, eject_after: 2, readmit_after: 2 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Queue {
    pub max_depth: usize,
}
impl Default for Queue {
    fn default() -> Self {
        Self { max_depth: 64 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retry {
    pub max_attempts: u32,
    pub backoff: Vec<f64>,
}
impl Default for Retry {
    fn default() -> Self {
        Self { max_attempts: 3, backoff: vec![0.5, 1.0, 2.0] }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendCfg {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentCfg {
    pub name: String,
    pub served_models: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Per backend: the most requests one backend gets for this deployment at a time.
    pub cap: u32,
    #[serde(default)]
    pub rewrites: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Remove,
    FilterArray,
    Merge,
    SetDefault,
    RenameModel,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleCfg {
    pub name: String,
    pub paths: Vec<String>,
    pub op: OpKind,
    #[serde(default)]
    pub pointer: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub keep_where: Option<BTreeMap<String, Value>>,
    /// rename_model: the new name.
    #[serde(default)]
    pub to: Option<String>,
    /// rename_model: only rename these names (default: any).
    #[serde(default)]
    pub from: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCfg {
    pub name: String,
    /// Extra listener for this profile (dgx-lb compatibility). 0 = ephemeral (tests).
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub rewrites: Vec<String>,
}

pub const DEFAULT_PROFILE: &str = "default";

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| format!("config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &str) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("config {path}: {e}"))?;
        Self::from_toml(&text)
    }

    pub fn listen_addr(&self) -> Result<SocketAddr, String> {
        self.listen.parse().map_err(|e| format!("config: listen {:?}: {e}", self.listen))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.listen_addr()?;
        if self.backends.is_empty() {
            return Err("config: at least one [[backend]] is required".into());
        }
        if self.deployments.is_empty() {
            return Err("config: at least one [[deployment]] is required".into());
        }
        unique("backend", self.backends.iter().map(|b| b.name.as_str()))?;
        for b in &self.backends {
            if b.name == "fleet" || b.name == "none" {
                return Err("config: backend names 'fleet' and 'none' are reserved".into());
            }
            crate::client::Upstream::parse(&b.url).map_err(|e| format!("config: backend {}: {e}", b.name))?;
        }
        unique("rule", self.rules.iter().map(|r| r.name.as_str()))?;
        for r in &self.rules {
            crate::rules::Rule::from_cfg(r)?;
        }
        let rule_names: HashSet<&str> = self.rules.iter().map(|r| r.name.as_str()).collect();
        let check_refs = |owner: &str, refs: &[String]| -> Result<(), String> {
            for name in refs {
                if !rule_names.contains(name.as_str()) {
                    return Err(format!("config: {owner} refers to unknown rule {name:?}"));
                }
            }
            Ok(())
        };
        unique("deployment", self.deployments.iter().map(|d| d.name.as_str()))?;
        unique(
            "model name (served_models + aliases across deployments)",
            self.deployments.iter().flat_map(|d| d.served_models.iter().chain(d.aliases.iter()).map(String::as_str)),
        )?;
        for d in &self.deployments {
            if d.served_models.is_empty() {
                return Err(format!("config: deployment {} has no served_models", d.name));
            }
            if d.cap == 0 {
                return Err(format!("config: deployment {} has cap 0", d.name));
            }
            check_refs(&format!("deployment {}", d.name), &d.rewrites)?;
        }
        unique("profile", self.profiles.iter().map(|p| p.name.as_str()))?;
        for p in &self.profiles {
            if p.name.is_empty() || p.name.contains('/') {
                return Err(format!("config: bad profile name {:?}", p.name));
            }
            check_refs(&format!("profile {}", p.name), &p.rewrites)?;
        }
        let t = &self.timeouts;
        if [t.connect, t.first_byte, t.idle, t.total].iter().any(|v| v.is_nan() || *v <= 0.0) {
            return Err("config: every timeout must be > 0".into());
        }
        if self.retry.max_attempts == 0 {
            return Err("config: retry.max_attempts must be >= 1".into());
        }
        if self.health.interval <= 0.0 || self.health.probe_timeout <= 0.0 {
            return Err("config: health.interval and health.probe_timeout must be > 0".into());
        }
        Ok(())
    }

    /// BP-60 floor.
    pub fn check_timeouts(&self, allow_short: bool) -> Result<(), String> {
        let t = &self.timeouts;
        if !allow_short && (t.total < MIN_TOTAL_S || t.first_byte < MIN_FIRST_BYTE_S) {
            return Err(format!(
                "refusing timeouts total={} first_byte={} (minimum {MIN_TOTAL_S}/{MIN_FIRST_BYTE_S} s; BP-60). \
                 Pass --i-know-short-timeouts to override.",
                t.total, t.first_byte
            ));
        }
        Ok(())
    }

    pub fn ledger_path(&self) -> std::path::PathBuf {
        expand_home(&self.ledger_dir)
    }
}

pub fn expand_home(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::Path::new(&home).join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

fn unique<'a>(what: &str, names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = HashSet::new();
    for n in names {
        if !seen.insert(n) {
            return Err(format!("config: duplicate {what} {n:?}"));
        }
    }
    Ok(())
}
