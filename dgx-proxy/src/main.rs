use std::process::ExitCode;

const USAGE: &str = "usage: dgx-proxy --config <path> [--i-know-short-timeouts] [--check]

  --config PATH              TOML config (see config.example.toml)
  --i-know-short-timeouts    allow total < 600 s or first_byte < 60 s (tests/dev only; BP-60)
  --check                    validate the config and exit";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut config, mut short, mut check) = (None, false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = args.next(),
            "--i-know-short-timeouts" => short = true,
            "--check" => check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("unknown argument {a:?}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(path) = config else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let cfg = match dgx_proxy::Config::load(&path).and_then(|c| c.check_timeouts(short).map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if check {
        println!("{{\"event\":\"config_ok\"}}");
        return ExitCode::SUCCESS;
    }
    let running = match dgx_proxy::start(cfg, short).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let (ok, health) = running.app.health();
    println!(
        "{}",
        serde_json::json!({"event": "started", "ports": running.ports, "eligible": ok,
                           "deployments": health["deployments"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>())})
    );
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("signal: {e}");
            return ExitCode::from(1);
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    drop(running);
    println!("{{\"event\":\"stopped\"}}");
    ExitCode::SUCCESS
}
