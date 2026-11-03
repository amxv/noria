mod accounts;
mod client;
mod daemon;
mod desktop;
mod status;
mod update;
#[cfg(windows)]
mod windows_update;

use crate::{config, integration, service};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub async fn run(mut args: Vec<String>) -> Result<()> {
    #[cfg(windows)]
    service::disable_implicit_stdio_inheritance()?;
    let mut selected = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            break;
        }
        if args[index] == "--home" {
            ensure!(index + 1 < args.len(), "--home requires a directory");
            selected = Some(PathBuf::from(args.remove(index + 1)));
            args.remove(index);
        } else if let Some(home) = args[index].strip_prefix("--home=") {
            ensure!(!home.is_empty(), "--home requires a directory");
            selected = Some(PathBuf::from(home));
            args.remove(index);
        } else {
            index += 1;
        }
    }
    if args.is_empty() || is_help(&args[0]) {
        print!("{HELP}");
        return Ok(());
    }
    if args[0] == "--version" {
        ensure!(args.len() == 1, "usage: noria --version");
        println!("noria {}", crate::VERSION);
        return Ok(());
    }
    let home = match selected {
        Some(path) => std::path::absolute(path)?,
        None => config::home()?,
    };
    let command = args.remove(0);
    match command.as_str() {
        "__daemon" => {
            no_args(&args, "internal daemon")?;
            daemon::serve(&home).await
        }
        "__supervise" => {
            no_args(&args, "internal supervisor")?;
            service::supervise(&home).await
        }
        "__login" => {
            no_args(&args, "internal login")?;
            if config::load(&home)?.start_at_login {
                lifecycle(&home, "start", vec!["--json".into()]).await?;
            }
            Ok(())
        }
        "__sync-updates" => {
            no_args(&args, "internal update sync")?;
            config::init(&home)?;
            desktop::sync_updates(&home, &service::executable()?).await
        }
        "__menubar-reopen" => desktop::delayed_menu(&home, args).await,
        "__tray-reopen" => desktop::delayed_tray(&home, args).await,
        "__apply-windows-upgrade" => {
            #[cfg(windows)]
            {
                windows_update::apply(&home, args).await
            }
            #[cfg(not(windows))]
            {
                let _ = args;
                bail!("Windows upgrade helper is Windows-only")
            }
        }
        "init" => {
            if help_requested(&args) {
                println!(
                    "Usage: noria init\nCreate a private home and local gateway secret. Existing accounts are preserved."
                );
                return Ok(());
            }
            no_args(&args, "noria init")?;
            config::init(&home)?;
            println!("Initialized {}", home.display());
            Ok(())
        }
        "auth-token" => {
            no_args(&args, "noria auth-token")?;
            println!("{}", config::secret(&home)?);
            Ok(())
        }
        "auth" => {
            ensure!(
                args == ["token"],
                "usage: noria auth token [--home <NORIA_HOME>]"
            );
            println!("{}", config::secret(&home)?);
            Ok(())
        }
        "account" => accounts::command(&home, args).await,
        "start" | "stop" | "restart" => lifecycle(&home, &command, args).await,
        "status" => status::command(&home, args).await,
        "health" => status::health(&home, args).await,
        "doctor" => status::doctor(&home, args),
        "quota" => daemon::quota(&home, args).await,
        "preferences" => desktop::preferences(&home, args).await,
        "menubar" => desktop::menubar(&home, args).await,
        "tray" => desktop::tray(&home, args).await,
        "integrate" => client::integrate(&home, args).await,
        "integrations" => client::integrations(&home, args),
        "env" | "run" => client::command(&home, &command, args).await,
        "upgrade" => update::command(&home, args).await,
        _ => bail!("unknown command {command:?} (run noria --help)"),
    }
}

pub(super) fn print_json(value: &impl Serialize) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}
pub(super) fn is_help(value: &str) -> bool {
    matches!(value, "help" | "--help" | "-h")
}
pub(super) fn help_requested(args: &[String]) -> bool {
    args.iter().any(|v| is_help(v))
}
pub(super) fn no_args(args: &[String], usage: &str) -> Result<()> {
    ensure!(args.is_empty(), "usage: {usage}");
    Ok(())
}
pub(super) fn flag(args: &mut Vec<String>, name: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != name);
    args.len() != before
}
pub(super) fn option(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let mut value = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == name {
            ensure!(index + 1 < args.len(), "{name} requires a value");
            value = Some(args.remove(index + 1));
            args.remove(index);
        } else if let Some(v) = args[index].strip_prefix(&format!("{name}=")) {
            value = Some(v.to_owned());
            args.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(value)
}
pub(super) fn daemon_address(home: &Path, fallback: &str) -> String {
    std::fs::read(home.join("daemon.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["address"].as_str().map(str::to_owned))
        .filter(|a| config::validate_listen(a).is_ok())
        .unwrap_or_else(|| fallback.to_owned())
}
pub(super) async fn control(
    home: &Path,
    method: reqwest::Method,
    path: &str,
    payload: Option<&Value>,
) -> Result<Value> {
    let timeout = match path {
        "/_noria/refresh" => Duration::from_secs(120),
        "/_noria/health" => Duration::from_millis(700),
        _ => Duration::from_secs(2),
    };
    control_timeout(home, method, path, payload, timeout).await
}
pub(super) async fn control_timeout(
    home: &Path,
    method: reqwest::Method,
    path: &str,
    payload: Option<&Value>,
    timeout: Duration,
) -> Result<Value> {
    let c = config::load(home)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()?;
    let mut request = client
        .request(
            method,
            format!("http://{}{path}", daemon_address(home, &c.listen)),
        )
        .bearer_auth(config::secret(home)?)
        .header("Content-Type", "application/json");
    if let Some(payload) = payload {
        request = request.json(payload);
    }
    let mut response = request
        .send()
        .await
        .context("daemon unavailable (run noria start)")?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "daemon control returned HTTP {}",
        response.status().as_u16()
    );
    ensure!(
        response.content_length().is_none_or(|n| n <= 4 << 20),
        "oversized daemon control response"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 4 << 20,
            "oversized daemon control response"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub(super) async fn running(home: &Path) -> bool {
    control(home, reqwest::Method::GET, "/_noria/health", None)
        .await
        .is_ok()
}
pub(super) async fn reload(home: &Path) {
    let _ = control(home, reqwest::Method::POST, "/_noria/reload", None).await;
}

async fn lifecycle(home: &Path, op: &str, mut args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!("Usage: noria {op} [--json]");
        return Ok(());
    }
    let as_json = flag(&mut args, "--json");
    no_args(&args, &format!("noria {op} [--json]"))?;
    config::init(home)?;
    let _lock = config::lock(home, "lifecycle")?;
    if matches!(op, "stop" | "restart") {
        stop(home).await?;
    }
    if matches!(op, "start" | "restart") {
        start(home).await?;
    }
    let message = if op == "stop" {
        "Noria stopped. Previous direct provider routing and login restored. Reopen existing clients to reload settings."
    } else {
        "Noria is running. Configured clients use the account pool."
    };
    if as_json {
        print_json(&json!({"running":op!="stop","message":message,"client_restart_required":true}))
    } else {
        println!("{message}");
        Ok(())
    }
}
pub(super) async fn start(home: &Path) -> Result<()> {
    start_at(home, &service::executable()?).await
}
pub(super) async fn start_at(home: &Path, exe: &Path) -> Result<()> {
    let c = config::load(home)?;
    if !running(home).await {
        service::start(home, exe).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            if running(home).await {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = service::stop(home, exe).await;
                bail!(
                    "Noria did not become healthy; inspect noria health and the private logs/service.log"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // Starting an already-running service must also restore persistent intent.
    service::set_intent(home, exe, true)?;
    integration::set_active(home, exe, &daemon_address(home, &c.listen), true)
}
pub(super) async fn stop(home: &Path) -> Result<()> {
    let c = config::load(home)?;
    let exe = service::executable()?;
    integration::set_active(home, &exe, &c.listen, false)?;
    stop_runtime(home, &exe).await
}
pub(super) async fn stop_runtime(home: &Path, exe: &Path) -> Result<()> {
    service::set_intent(home, exe, false)?;
    let _ = control(home, reqwest::Method::POST, "/_noria/shutdown", None).await;
    service::stop(home, exe).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        if !running(home).await
            && let Ok(guard) = config::lock_timeout(home, "daemon", Duration::from_millis(50))
        {
            drop(guard);
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "stop requested but requests are still draining; check noria status"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const HELP: &str = "Noria: local native Codex and Claude account gateway\n\nUsage: noria <command> [arguments]\n       noria --version\n\nCommands:\n  init                         create private configuration\n  account add|relogin|list|rename|remove|enable|disable\n  start | stop | restart       background routing; stop restores direct settings\n  integrate codex|claude        preview and install persistent client routing\n  integrations [--json]         inspect configured client profiles\n  preferences [key on|off]      login and automatic update settings\n  menubar open|install          native macOS menu bar\n  tray open|quit                native Windows/Linux tray app\n  upgrade [--check]             verified CLI and desktop upgrades\n  health [--json]               check gateway and client configuration\n  status [--json]               inspect accounts and routing\n  quota refresh [account]       read native quota without a prompt\n  doctor                       check local prerequisites\n  env codex|claude              print child-only routing configuration\n  run codex|claude -- [args]    run a client through Noria\n\nUse <command> --help for command details. NORIA_HOME defaults to ~/.noria.\nAccount login is explicit. Noria never imports your existing credentials.\n";
