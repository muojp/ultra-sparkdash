//! llm-quickbench — concurrency sweep against any OpenAI-compatible endpoint.
//!
//! Rewritten from the Python original because the client became the bottleneck: measuring two
//! servers from one process, the pool reached only 1.36x a single node while its per-node latency
//! showed the servers still had room. A sweep whose own client saturates first measures the client.
//!
//! Several endpoints are treated as one pool: requests go round-robin and the aggregate is the
//! pool's. Prompts are unique per request by default, because a shared prefix is served from the
//! prefix cache and reports throughput the server cannot sustain.

mod scenarios;
mod sse;
mod stats;

use clap::Parser;
use futures_util::StreamExt;
use scenarios::{build_prompt, load_cases, scenario, Case, Scenario, SCENARIOS};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Parser, Debug, Clone)]
#[command(about = "Concurrency sweep for an OpenAI-compatible server")]
struct Args {
    /// Base URL(s) ending in /v1, comma-separated. Several are treated as one pool.
    #[arg(long, default_value = "http://192.168.0.100:8888/v1")]
    api: String,
    /// Served model id (default: the first endpoint's answer, which the rest must match).
    #[arg(long)]
    model: Option<String>,
    #[arg(short = 'c', long, default_value = "1,2,4,8")]
    concurrency: String,
    /// Override the scenario's output length (0 = per-scenario default).
    #[arg(long, default_value_t = 0)]
    max_tokens: u32,
    #[arg(long, default_value_t = 512)]
    prompt_tokens: u32,
    #[arg(long, default_value = "chat")]
    scenario: String,
    /// Send chat_template_kwargs={"thinking": false}; dropped after the first rejection.
    #[arg(long = "no-thinking", action = clap::ArgAction::SetFalse, default_value_t = true)]
    thinking: bool,
    /// Send every request the same prompt (measures the prefix-cache path).
    #[arg(long, default_value_t = false)]
    shared_prefix: bool,
    #[arg(long, default_value_t = 1)]
    warmup: u32,
    #[arg(long, default_value_t = 900)]
    timeout: u64,
    #[arg(long = "json")]
    json_out: Option<PathBuf>,
    /// Condition to record with the run (e.g. "taken during a weight download").
    #[arg(long, default_value = "")]
    note: String,
    #[arg(long)]
    cases: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct Outcome {
    ok: bool,
    err: Option<String>,
    ttft: Option<f64>,
    wall: f64,
    completion: u64,
    prompt: u64,
    reasoning: u64,
    cached: u64,
    finish: Option<String>,
    text: String,
    reasoning_text: String,
    endpoint: String,
}

fn endpoints(api: &str) -> Vec<String> {
    api.split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

async fn discover_model(client: &reqwest::Client, apis: &[String]) -> Result<String, String> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for a in apis {
        let url = format!("{a}/models");
        let v: Value = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let id = v["data"][0]["id"]
            .as_str()
            .ok_or_else(|| format!("no model served at {a}"))?;
        seen.insert(a.clone(), id.to_string());
    }
    let first = seen.values().next().cloned().unwrap_or_default();
    if seen.values().any(|m| *m != first) {
        // A pool of two different models is a mistake, not a configuration.
        return Err(format!("endpoints serve different models: {seen:?}"));
    }
    Ok(first)
}

#[allow(clippy::too_many_arguments)]
async fn one_request(
    client: &reqwest::Client,
    api: &str,
    model: &str,
    prompt: String,
    max_tokens: u32,
    thinking: bool,
    scen: &'static Scenario,
    keep_text: bool,
    thinking_unsupported: Arc<Mutex<bool>>,
) -> Outcome {
    let mut out = Outcome { endpoint: api.to_string(), ..Default::default() };
    let send_thinking = !thinking && !*thinking_unsupported.lock().await;
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if send_thinking {
        body["chat_template_kwargs"] = json!({"thinking": false});
    }
    if scen.json_schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "os", "strict": true, "schema": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "year": {"type": "integer"},
                               "languages": {"type": "array", "items": {"type": "string"}}},
                "required": ["name", "year", "languages"], "additionalProperties": false}}
        });
    }

    let t0 = Instant::now();
    let resp = client
        .post(format!("{api}/chat/completions"))
        .json(&body)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            out.wall = t0.elapsed().as_secs_f64();
            out.err = Some(e.to_string());
            return out;
        }
    };
    if resp.status() == reqwest::StatusCode::BAD_REQUEST && send_thinking {
        // Templates differ between models; a sweep must not die on the first 400.
        let mut flag = thinking_unsupported.lock().await;
        if !*flag {
            *flag = true;
            eprintln!("# this model rejects chat_template_kwargs={{'thinking': false}}; continuing without it");
        }
        drop(flag);
        return Box::pin(one_request(
            client, api, model, body["messages"][0]["content"].as_str().unwrap_or("").to_string(),
            max_tokens, true, scen, keep_text, thinking_unsupported,
        ))
        .await;
    }
    if !resp.status().is_success() {
        out.wall = t0.elapsed().as_secs_f64();
        out.err = Some(format!("HTTP {}", resp.status()));
        return out;
    }

    let mut state = sse::StreamState::default();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                out.err = Some(e.to_string());
                break;
            }
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find('\n') {
            let line: String = buf.drain(..=idx).collect();
            match state.feed(line.trim_end(), keep_text) {
                sse::Event::Token { first: true } => out.ttft = Some(t0.elapsed().as_secs_f64()),
                sse::Event::Done => {}
                _ => {}
            }
        }
    }
    out.wall = t0.elapsed().as_secs_f64();
    if let Some(u) = state.usage {
        out.completion = u.completion;
        out.prompt = u.prompt;
        out.reasoning = u.reasoning;
        out.cached = u.cached;
    }
    out.finish = state.finish;
    out.text = state.text.chars().take(6000).collect();
    out.reasoning_text = state.reasoning_text.chars().take(6000).collect();
    out.ok = out.err.is_none();
    out
}

#[allow(clippy::too_many_arguments)]
async fn run_level(
    client: &reqwest::Client,
    apis: &[String],
    model: &str,
    level: usize,
    args: &Args,
    seed_base: u64,
    scen: &'static Scenario,
    cases: &[Case],
    thinking_unsupported: Arc<Mutex<bool>>,
) -> Value {
    let max_tokens = if args.max_tokens > 0 { args.max_tokens } else { scen.max_tokens };
    let keep_text = scen.name == "review";
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(level);
    for i in 0..level {
        let seed = if args.shared_prefix { 0 } else { seed_base + i as u64 };
        let prompt = build_prompt(args.prompt_tokens, seed, scen, cases);
        let api = apis[i % apis.len()].clone();
        let client = client.clone();
        let model = model.to_string();
        let thinking = args.thinking;
        let flag = thinking_unsupported.clone();
        let case_id = if keep_text && !cases.is_empty() {
            Some(cases[(seed as usize) % cases.len()].id.clone())
        } else {
            None
        };
        tasks.push(tokio::spawn(async move {
            let o = one_request(&client, &api, &model, prompt, max_tokens, thinking, scen, keep_text, flag).await;
            (o, case_id)
        }));
    }
    let mut results = Vec::with_capacity(level);
    for t in tasks {
        if let Ok(r) = t.await {
            results.push(r);
        }
    }
    let wall = t0.elapsed().as_secs_f64();

    let ok: Vec<&Outcome> = results.iter().map(|(o, _)| o).filter(|o| o.ok).collect();
    let failed = results.len() - ok.len();
    if ok.is_empty() {
        return json!({
            "scenario": scen.name, "concurrency": level, "wall_s": stats::round2(wall),
            "failed": failed,
            "error": results.iter().find_map(|(o, _)| o.err.clone()).unwrap_or_default(),
        });
    }

    let total_out: u64 = ok.iter().map(|o| o.completion).sum();
    let mut ttfts: Vec<f64> = ok.iter().filter_map(|o| o.ttft).collect();
    ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Per-stream decode excludes the prefill the caller waited through.
    let mut rates: Vec<f64> = ok
        .iter()
        .filter_map(|o| {
            let t = o.ttft?;
            let d = o.wall - t;
            (d > 0.05 && o.completion > 0).then(|| o.completion as f64 / d)
        })
        .collect();
    let mut lats: Vec<f64> = ok.iter().map(|o| o.wall).collect();
    let mut reasoning: Vec<f64> = ok.iter().map(|o| o.reasoning as f64).collect();
    let mut cached: Vec<f64> = ok.iter().map(|o| o.cached as f64).collect();

    let mut per_endpoint: Map<String, Value> = Map::new();
    for (o, _) in results.iter().filter(|(o, _)| o.ok) {
        let e = per_endpoint.entry(o.endpoint.clone()).or_insert(json!(0));
        *e = json!(e.as_u64().unwrap_or(0) + o.completion);
    }
    let answers: Vec<Value> = results
        .iter()
        .filter(|(o, _)| o.ok)
        .filter_map(|(o, cid)| {
            cid.as_ref().map(|c| json!({
                "case": c, "text": o.text, "reasoning": o.reasoning_text, "finish": o.finish
            }))
        })
        .collect();
    let mut finish: Vec<String> = ok.iter().filter_map(|o| o.finish.clone()).collect();
    finish.sort();
    finish.dedup();

    json!({
        "scenario": scen.name,
        "answers": answers,
        "concurrency": level,
        "endpoints": per_endpoint,
        "requests": ok.len(),
        "failed": failed,
        "wall_s": stats::round2(wall),
        "output_tokens": total_out,
        "aggregate_tok_s": stats::round1(total_out as f64 / wall),
        "per_stream_tok_s": stats::median(&mut rates).map(stats::round1),
        "ttft_p50_s": stats::percentile(&ttfts, 50.0).map(stats::round2),
        "ttft_p95_s": stats::percentile(&ttfts, 95.0).map(stats::round2),
        "latency_p50_s": stats::median(&mut lats).map(stats::round2),
        "prompt_tokens": ok[0].prompt,
        "cached_tokens_p50": stats::median(&mut cached).unwrap_or(0.0) as u64,
        "reasoning_tokens_p50": stats::median(&mut reasoning).unwrap_or(0.0) as u64,
        "finish_reasons": finish,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let apis = endpoints(&args.api);
    if apis.is_empty() {
        eprintln!("no endpoints in --api");
        std::process::exit(2);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(args.timeout))
        .pool_max_idle_per_host(64)
        .build()?;

    let model = match &args.model {
        Some(m) => m.clone(),
        None => match discover_model(&client, &apis).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        },
    };
    let cases = load_cases(args.cases.as_deref());

    let names: Vec<&'static Scenario> = if args.scenario == "all" {
        SCENARIOS.iter().collect()
    } else {
        let mut v = Vec::new();
        for n in args.scenario.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match scenario(n) {
                Some(s) => v.push(s),
                None => {
                    eprintln!(
                        "unknown scenario {n:?} (have: {})",
                        SCENARIOS.iter().map(|s| s.name).collect::<Vec<_>>().join(",")
                    );
                    std::process::exit(2);
                }
            }
        }
        v
    };
    let levels: Vec<usize> = args
        .concurrency
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();

    println!("api:    {}", apis.join(", "));
    println!("model:  {model}");
    println!(
        "params: max_tokens={} prompt~{} tok thinking={} prompts={} scenarios={}",
        if args.max_tokens > 0 { args.max_tokens.to_string() } else { "per-scenario".into() },
        args.prompt_tokens,
        args.thinking,
        if args.shared_prefix { "shared" } else { "unique" },
        args.scenario
    );
    println!();

    let thinking_unsupported = Arc::new(Mutex::new(false));
    for i in 0..args.warmup {
        for a in &apis {
            let chat = scenario("chat").unwrap();
            let _ = one_request(
                &client, a, &model, build_prompt(args.prompt_tokens, 90_000 + i as u64, chat, &cases),
                64, args.thinking, chat, false, thinking_unsupported.clone(),
            )
            .await;
        }
    }

    println!(
        "{:<11} {:>4} {:>5} {:>8} {:>8} {:>10} {:>11} {:>9} {:>9} {:>8} {:>11}",
        "scenario", "conc", "reqs", "wall_s", "out_tok", "aggregate", "per-stream", "ttft_p50",
        "ttft_p95", "lat_p50", "reason_p50"
    );
    let mut rows: Vec<Value> = Vec::new();
    let mut seed: u64 = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
        % 10_000;
    for scen in &names {
        for lvl in &levels {
            let row = run_level(
                &client, &apis, &model, *lvl, &args, seed, scen, &cases, thinking_unsupported.clone(),
            )
            .await;
            seed += *lvl as u64;
            if let Some(err) = row.get("error").and_then(|e| e.as_str()) {
                println!("{:<11} {:>4}  FAILED: {err}", scen.name, lvl);
                rows.push(row);
                continue;
            }
            let g = |k: &str| -> String {
                match row.get(k) {
                    Some(Value::Null) | None => "—".into(),
                    Some(v) => v.to_string().trim_matches('"').to_string(),
                }
            };
            println!(
                "{:<11} {:>4} {:>5} {:>8} {:>8} {:>10} {:>11} {:>9} {:>9} {:>8} {:>11}",
                scen.name, lvl, g("requests"), g("wall_s"), g("output_tokens"), g("aggregate_tok_s"),
                g("per_stream_tok_s"), g("ttft_p50_s"), g("ttft_p95_s"), g("latency_p50_s"),
                g("reasoning_tokens_p50")
            );
            if apis.len() > 1 {
                if let Some(m) = row.get("endpoints").and_then(|e| e.as_object()) {
                    let parts: Vec<String> = m
                        .iter()
                        .map(|(k, v)| {
                            let host = k.split("//").nth(1).unwrap_or(k).split('/').next().unwrap_or(k);
                            format!("{host} {v} tok")
                        })
                        .collect();
                    println!("     per endpoint: {}", parts.join(", "));
                }
            }
            rows.push(row);
        }
    }

    println!();
    for scen in &names {
        let good: Vec<&Value> = rows
            .iter()
            .filter(|r| r["scenario"] == scen.name && r.get("aggregate_tok_s").is_some())
            .collect();
        if good.len() > 1 {
            let best = good
                .iter()
                .max_by(|a, b| {
                    a["aggregate_tok_s"].as_f64().unwrap_or(0.0)
                        .partial_cmp(&b["aggregate_tok_s"].as_f64().unwrap_or(0.0)).unwrap()
                })
                .unwrap();
            let base = good[0]["aggregate_tok_s"].as_f64().unwrap_or(1.0);
            println!(
                "{:<11} peak {} tok/s at concurrency {} ({:.2}x single), per-stream {} -> {}",
                scen.name, best["aggregate_tok_s"], best["concurrency"],
                best["aggregate_tok_s"].as_f64().unwrap_or(0.0) / base,
                good[0]["per_stream_tok_s"], good[good.len() - 1]["per_stream_tok_s"]
            );
        }
    }

    if let Some(path) = &args.json_out {
        let doc = json!({
            "api": args.api,
            "endpoints": apis,
            "model": model,
            "note": args.note,
            "client": "rust",
            "finished_at": chrono_now(),
            "params": {
                "max_tokens": args.max_tokens,
                "scenarios": names.iter().map(|s| s.name).collect::<Vec<_>>(),
                "prompt_tokens": args.prompt_tokens,
                "thinking": args.thinking,
                "shared_prefix": args.shared_prefix,
            },
            "rows": rows,
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Minimal UTC stamp without pulling in a date crate for one line.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    format!("epoch+{days}d{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}
