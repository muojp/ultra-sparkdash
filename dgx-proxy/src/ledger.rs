//! JSONL ledger: one row per request, one file per UTC day. Bodies are never written.

use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub ts: f64,
    pub rid: String,
    pub profile: String,
    pub dispatch_id: Option<String>,
    pub method: String,
    pub path: String,
    pub stream: Option<bool>,
    pub deployment: Option<String>,
    pub model_in: Option<String>,
    pub model_out: Option<String>,
    pub rewrites: Vec<String>,
    pub node: Option<String>,
    pub attempts: u32,
    pub status: Option<u16>,
    pub outcome: Option<String>,
    pub queue_wait_s: f64,
    pub headers_s: Option<f64>,
    pub ttft_s: Option<f64>,
    pub wall_s: Option<f64>,
    pub bytes: u64,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub errors: Vec<String>,
}

pub struct Ledger {
    pub dir: PathBuf,
    lock: Mutex<()>,
}

impl Ledger {
    pub fn new(dir: PathBuf) -> std::io::Result<Ledger> {
        std::fs::create_dir_all(&dir)?;
        Ok(Ledger { dir, lock: Mutex::new(()) })
    }

    pub fn write(&self, row: &Row) {
        let mut line = match serde_json::to_string(row) {
            Ok(s) => s,
            Err(_) => return,
        };
        line.push('\n');
        let path = self.dir.join(format!("ledger-{}.jsonl", utc_ymd(SystemTime::now())));
        let _g = self.lock.lock().unwrap();
        let res = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = res {
            eprintln!("{{\"event\":\"ledger_error\",\"error\":{:?}}}", e.to_string());
        }
    }
}

/// "YYYYMMDD" in UTC (civil-from-days, H. Hinnant).
pub fn utc_ymd(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ymd() {
        assert_eq!(utc_ymd(UNIX_EPOCH), "19700101");
        // 2026-10-05T12:00:00Z
        assert_eq!(utc_ymd(UNIX_EPOCH + Duration::from_secs(1_791_201_600)), "20261005");
        // 2024-02-29T23:59:59Z
        assert_eq!(utc_ymd(UNIX_EPOCH + Duration::from_secs(1_709_251_199)), "20240229");
    }
}
