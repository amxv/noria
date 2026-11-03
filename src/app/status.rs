use super::*;
use crate::{core::Scheduler, model::Window};
use chrono::{DateTime, Utc};

pub(super) async fn dashboard(home: &Path) -> Result<Value> {
    let c = config::load(home)?;
    let live = control(home, reqwest::Method::GET, "/_noria/status", None).await;
    let is_running = live.is_ok();
    let mut value = match live {
        Ok(v) => v,
        Err(_) => {
            serde_json::to_value(Scheduler::new(home.join("scheduler.json"), c.accounts)?.status())?
        }
    };
    value["schema_version"] = json!(1);
    value["version"] = json!(crate::VERSION);
    value["running"] = json!(is_running);
    value["address"] = json!(daemon_address(home, &c.listen));
    value["integrations"] = serde_json::to_value(integration::views(home)?)?;
    value["client_roots"] = json!(client::native_roots());
    value["preferences"] = json!({"start_at_login":c.start_at_login,"menubar_at_login":c.menubar_at_login,"auto_update":c.auto_update});
    if !cfg!(target_os = "macos") {
        value["preferences"]["tray_at_login"] = json!(c.menubar_at_login);
    }
    value["observed_at"] = json!(Utc::now());
    Ok(value)
}
pub async fn command(home: &Path, mut args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!("Usage: noria status [--json]");
        return Ok(());
    }
    let as_json = flag(&mut args, "--json");
    no_args(&args, "noria status [--json]")?;
    let value = dashboard(home).await?;
    if as_json {
        return print_json(&value);
    }
    println!(
        "{}",
        if value["running"] == true {
            "Noria is running."
        } else {
            "Noria is stopped. Check noria integrations to inspect client routing."
        }
    );
    let accounts = value["accounts"]
        .as_array()
        .context("invalid status response")?;
    if accounts.is_empty() {
        println!("No accounts registered. Run: noria account add codex");
        return Ok(());
    }
    println!("ACCOUNT\tPROVIDER\t5H LEFT / RESET\tWEEK LEFT / RESET\tIN FLIGHT\tSTATE");
    let now = Utc::now();
    for account in accounts {
        let mut short = "unknown".to_owned();
        let mut week = short.clone();
        let mut state = "quota unknown".to_owned();
        if !account["snapshot"].is_null() {
            let snapshot = &account["snapshot"];
            if snapshot["available"] == true {
                state = "ready".into();
            }
            if snapshot["observed_at"]
                .as_str()
                .and_then(|v| v.parse::<DateTime<Utc>>().ok())
                .is_some_and(|at| now.signed_duration_since(at).num_seconds() > 120)
            {
                state = "stale quota".into();
            }
            if snapshot["auth_invalid"] == true {
                state = "login required".into();
            }
            if let Some(windows) = snapshot["windows"].as_array() {
                for item in windows {
                    if let Ok(window) = serde_json::from_value::<Window>(item.clone()) {
                        if !window.model.is_empty() {
                            continue;
                        }
                        if window.duration == 18_000_000_000_000 {
                            short = format_window(&window, now);
                        }
                        if window.duration == 604_800_000_000_000 {
                            week = format_window(&window, now);
                        }
                    }
                }
            }
        }
        if let Some(blockers) = account["blockers"].as_array() {
            for blocker in blockers {
                if blocker["until"]
                    .as_str()
                    .and_then(|v| v.parse::<DateTime<Utc>>().ok())
                    .is_some_and(|at| at <= now)
                {
                    continue;
                }
                if blocker["model"].as_str().is_some_and(|v| !v.is_empty()) {
                    state = "model limit".into();
                    continue;
                }
                let kind = blocker["kind"].as_str().unwrap_or_default();
                state = if kind == "auth" {
                    "login required".into()
                } else {
                    format!("{kind} blocked")
                };
            }
        }
        if account["enabled"] != true {
            state = "disabled".into();
        }
        let name = account["display_name"]
            .as_str()
            .or_else(|| account["id"].as_str())
            .unwrap_or_default();
        println!(
            "{name}\t{}\t{short}\t{week}\t{}\t{state}",
            account["provider"].as_str().unwrap_or_default(),
            account["inflight"].as_u64().unwrap_or(0)
        );
    }
    println!(
        "\n{} saved session binding(s). Existing sessions keep their account until an applicable limit.",
        value["bindings"].as_u64().unwrap_or(0)
    );
    println!("Use noria status --json for model-scoped quotas, timestamps, and routing metadata.");
    Ok(())
}
fn format_window(window: &Window, now: DateTime<Utc>) -> String {
    let remaining = window
        .used
        .map(|u| format!("{:.1}%", 100. - u))
        .unwrap_or_else(|| "?".into());
    let reset = match window.resets_at {
        Some(at) if at <= now => "refresh due".into(),
        Some(at) => compact_duration(at.signed_duration_since(now).num_seconds()),
        None if window.dormant => "not started".into(),
        None => "reset unknown".into(),
    };
    format!("{remaining} / {reset}")
}
fn compact_duration(seconds: i64) -> String {
    if seconds < 60 {
        return "<1m".into();
    }
    let minutes = (seconds + 30) / 60;
    let days = minutes / 1440;
    let hours = minutes % 1440 / 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if days == 0 && minutes % 60 > 0 {
        parts.push(format!("{}m", minutes % 60));
    }
    parts.join(" ")
}
pub(super) fn executable_available(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return true;
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            if candidate.with_extension(ext).is_file() {
                return true;
            }
        }
        false
    })
}
pub async fn health(home: &Path, mut args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!(
            "Usage: noria health [--json]\nChecks local service, configured routing and native executables. Sends no model prompt."
        );
        return Ok(());
    }
    let as_json = flag(&mut args, "--json");
    no_args(&args, "noria health [--json]")?;
    let c = config::load(home)?;
    let mut checks =
        vec![json!({"name":"Local router","ok":running(home).await,"detail":c.listen})];
    for v in integration::views(home)? {
        checks
            .push(json!({"name":format!("{} settings",v.provider),"ok":v.healthy,"detail":v.path}));
    }
    let mut providers = std::collections::HashSet::new();
    for a in c.accounts {
        if a.enabled && providers.insert(a.provider.clone()) {
            checks.push(json!({"name":format!("{} executable",a.provider),"ok":executable_available(&a.provider)}));
        }
    }
    for key in [
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        if std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
            checks.push(json!({"name":format!("Environment override: {key}"),"ok":false,"detail":"May bypass settings.json or override the gateway helper. Clear it for persistent routing."}));
        }
    }
    if as_json {
        return print_json(
            &json!({"ok":checks.iter().all(|v|v["ok"]==true),"checks":checks,"note":"Configuration and local reachability only; no live model request was sent. Reopen clients after routing changes."}),
        );
    }
    for check in checks {
        println!(
            "{}: {}",
            check["name"].as_str().unwrap_or_default(),
            check["ok"]
        );
    }
    Ok(())
}
fn private_permissions(path: &Path) -> bool {
    let Ok(info) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if info.file_type().is_symlink() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if info.permissions().mode() & 0o077 != 0 {
            return false;
        }
    }
    true
}
pub fn doctor(home: &Path, args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!(
            "Usage: noria doctor\nCheck configuration, permissions and native executables without reading live quotas or printing credentials."
        );
        return Ok(());
    }
    no_args(&args, "noria doctor")?;
    let c = config::load(home)?;
    let mut problems = 0;
    for kind in ["codex", "claude"] {
        if c.accounts.iter().any(|a| a.enabled && a.provider == kind) {
            let available = executable_available(kind);
            println!(
                "{kind}: {}",
                if available {
                    "available"
                } else {
                    "unavailable"
                }
            );
            if !available {
                problems += 1;
            }
        }
    }
    for path in [
        home.to_path_buf(),
        home.join("config.json"),
        home.join("secret"),
    ] {
        if !private_permissions(&path) {
            println!("private permissions: repair needed for {}", path.display());
            problems += 1;
        }
    }
    if config::secret(home).is_err() {
        println!("gateway secret: invalid or missing");
        problems += 1;
    }
    for a in c.accounts {
        let path = Path::new(&a.profile);
        if !path.is_dir() || !private_permissions(path) {
            println!("profile {}: missing or unsafe permissions", a.id);
            problems += 1;
        } else {
            println!("profile {}: isolated", a.id);
        }
    }
    ensure!(
        problems == 0,
        "doctor found {problems} prerequisite or permission issue(s)"
    );
    println!("Local prerequisites ready. Account login and real usage have not been tested.");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_quota_time_display() {
        let w = Window {
            used: Some(25.0),
            dormant: true,
            ..Default::default()
        };
        assert_eq!(format_window(&w, Utc::now()), "75.0% / not started");
        assert_eq!(compact_duration(90_000), "1d 1h");
    }
}
