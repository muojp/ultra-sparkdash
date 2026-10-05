//! Tiny Prometheus registry. Every proxy-own counter is incremented for its node AND for
//! node="fleet" in the same call, so the fleet series stays monotonic when a node disappears.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

pub const TTFT_BUCKETS: [f64; 12] = [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 900.0];

type Key = (String, Vec<(String, String)>);

#[derive(Default)]
struct Inner {
    counters: BTreeMap<Key, f64>,
    hist: BTreeMap<Key, Vec<f64>>, // bucket counts..., count, sum
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

fn key(name: &str, labels: &[(&str, &str)]) -> Key {
    let mut l: Vec<(String, String)> = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    l.sort();
    (name.to_string(), l)
}

fn with_fleet<'a>(labels: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    labels.iter().map(|&(k, v)| if k == "node" { (k, "fleet") } else { (k, v) }).collect()
}

impl Metrics {
    /// `fleet`: also add to the node="fleet" series (labels must contain `node`).
    pub fn inc(&self, name: &str, labels: &[(&str, &str)], value: f64, fleet: bool) {
        let mut g = self.inner.lock().unwrap();
        *g.counters.entry(key(name, labels)).or_insert(0.0) += value;
        if fleet {
            *g.counters.entry(key(name, &with_fleet(labels))).or_insert(0.0) += value;
        }
    }

    pub fn observe(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut g = self.inner.lock().unwrap();
        for lab in [labels.to_vec(), with_fleet(labels)] {
            let h = g.hist.entry(key(name, &lab)).or_insert_with(|| vec![0.0; TTFT_BUCKETS.len() + 2]);
            for (i, b) in TTFT_BUCKETS.iter().enumerate() {
                if value <= *b {
                    h[i] += 1.0;
                }
            }
            let n = TTFT_BUCKETS.len();
            h[n] += 1.0;
            h[n + 1] += value;
        }
    }

    pub fn get(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        self.inner.lock().unwrap().counters.get(&key(name, labels)).copied().unwrap_or(0.0)
    }

    pub fn render_into(&self, out: &mut String) {
        let g = self.inner.lock().unwrap();
        let mut last = "";
        for ((name, lab), v) in &g.counters {
            if name != last {
                let _ = writeln!(out, "# TYPE {name} counter");
                last = name;
            }
            let _ = writeln!(out, "{name}{} {}", fmt_labels(lab), num(*v));
        }
        let mut last = "";
        let n = TTFT_BUCKETS.len();
        for ((name, lab), h) in &g.hist {
            if name != last {
                let _ = writeln!(out, "# TYPE {name} histogram");
                last = name;
            }
            for (i, b) in TTFT_BUCKETS.iter().enumerate() {
                let mut l = lab.clone();
                l.push(("le".into(), num(*b)));
                let _ = writeln!(out, "{name}_bucket{} {}", fmt_labels(&l), num(h[i]));
            }
            let mut l = lab.clone();
            l.push(("le".into(), "+Inf".into()));
            let _ = writeln!(out, "{name}_bucket{} {}", fmt_labels(&l), num(h[n]));
            let _ = writeln!(out, "{name}_count{} {}", fmt_labels(lab), num(h[n]));
            let _ = writeln!(out, "{name}_sum{} {}", fmt_labels(lab), num(h[n + 1]));
        }
    }
}

pub fn fmt_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let inner: Vec<String> =
        labels.iter().map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))).collect();
    format!("{{{}}}", inner.join(","))
}

/// Like Python's `{:g}` for the values we emit: integers without a fraction.
pub fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Gauge block helper: `# TYPE` once, then the rows.
pub fn gauge(out: &mut String, name: &str, kind: &str, rows: &[(Vec<(String, String)>, f64)]) {
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (lab, v) in rows {
        let mut l = lab.clone();
        l.sort();
        let _ = writeln!(out, "{name}{} {}", fmt_labels(&l), num(*v));
    }
}
