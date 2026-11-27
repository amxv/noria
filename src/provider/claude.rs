use super::{
    Manager,
    credentials::bounded_response,
    process::{NativeProcess, command},
};
use crate::{
    core::NANOS_PER_MINUTE,
    model::{Account, Snapshot, Window},
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
#[error("Claude usage OAuth authorization rejected")]
struct Unauthorized;

#[derive(Debug, thiserror::Error)]
#[error("Claude is rate limiting usage checks; retrying at {until}")]
pub(crate) struct UsageRateLimit {
    pub until: DateTime<Utc>,
}

fn retry_at(value: &str, now: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = value
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .or_else(|| {
            httpdate::parse_http_date(value.trim())
                .ok()
                .map(|at| (DateTime::<Utc>::from(at) - now).num_seconds())
        })
        .unwrap_or(300);
    now + chrono::Duration::seconds(seconds.clamp(30, 3600))
}

impl Manager {
    pub(super) async fn claude_quota(&self, a: &Account, p: &Path) -> Result<Snapshot> {
        let started = Utc::now();
        let Ok((mut credential, expired)) = self.read_credential(a, p).await else {
            return self.claude_control_usage(a, p, started).await;
        };
        if expired {
            credential = self.rotate_usage_credential(a, p).await?;
        }
        let result = self
            .claude_oauth_usage(&a.id, &credential.token, started)
            .await;
        if matches!(&result, Err(error) if error.is::<Unauthorized>()) {
            let credential = self.rotate_usage_credential(a, p).await?;
            return self
                .claude_oauth_usage(&a.id, &credential.token, Utc::now())
                .await;
        }
        result
    }

    async fn rotate_usage_credential(
        &self,
        a: &Account,
        p: &Path,
    ) -> Result<crate::model::Credential> {
        self.refresh_claude_oauth(a, p)
            .await
            .context("Claude usage credential refresh failed; sign in again")?;
        let (credential, expired) = self.read_credential(a, p).await?;
        anyhow::ensure!(
            !expired,
            "Claude OAuth credential refresh was not persisted; sign in again"
        );
        Ok(credential)
    }
    async fn claude_control_usage(
        &self,
        a: &Account,
        p: &Path,
        started: DateTime<Utc>,
    ) -> Result<Snapshot> {
        let mut cmd = command(
            &self.claude_command,
            p,
            "claude",
            &[
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--setting-sources=",
                "--tools",
                "",
                "--no-session-persistence",
            ],
        );
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-go")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        let mut process = NativeProcess::spawn(cmd, 1 << 20).map_err(|_| {
            anyhow::anyhow!("cannot start Claude Code; install the native claude executable")
        })?;
        process.write(&json!({"request_id":"noria_initialize","type":"control_request","request":{"subtype":"initialize"}})).await?;
        let mut initialized = false;
        loop {
            let message = process.read().await?;
            let kind = message
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind != "control_response" {
                if ["assistant", "user", "result", "control_request"].contains(&kind) {
                    bail!("unexpected Claude activity during passive quota read");
                }
                continue;
            }
            if message.pointer("/response/subtype").and_then(Value::as_str) != Some("success") {
                bail!("Claude control request was rejected");
            }
            match message
                .pointer("/response/request_id")
                .and_then(Value::as_str)
            {
                Some("noria_initialize") => {
                    initialized = true;
                    process.write(&json!({"request_id":"noria_usage","type":"control_request","request":{"subtype":"get_usage","skip_behaviors":true}})).await?;
                }
                Some("noria_usage") => {
                    if !initialized {
                        bail!("Claude usage response preceded initialization");
                    }
                    let data = message
                        .pointer("/response/response")
                        .ok_or_else(|| anyhow::anyhow!("Claude usage control returned no data"))?;
                    return parse_claude(&a.id, &serde_json::to_vec(data)?, started);
                }
                _ => {}
            }
        }
    }
    async fn claude_oauth_usage(
        &self,
        id: &str,
        token: &str,
        started: DateTime<Utc>,
    ) -> Result<Snapshot> {
        let resp = self
            .usage_client
            .get(&self.claude_usage_url)
            .bearer_auth(token)
            .header("Anthropic-Beta", "oauth-2025-04-20")
            .header("Accept", "application/json")
            .header("User-Agent", "claude-code")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Claude subscription usage endpoint unavailable"))?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(Unauthorized.into());
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let value = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                return Err(UsageRateLimit {
                    until: retry_at(value, Utc::now()),
                }
                .into());
            }
            bail!(
                "Claude subscription usage returned HTTP {}",
                status.as_u16()
            );
        }
        let data = bounded_response(resp, 64 << 10)
            .await
            .map_err(|_| anyhow::anyhow!("invalid Claude subscription usage response size"))?;
        let limits: Value = serde_json::from_slice(&data)
            .map_err(|_| anyhow::anyhow!("invalid Claude subscription usage response"))?;
        if !limits.is_object() {
            bail!("invalid Claude subscription usage response");
        }
        let wrapped =
            serde_json::to_vec(&json!({"rate_limits_available":true,"rate_limits":limits}))?;
        let snapshot = parse_claude(id, &wrapped, started)?;
        if snapshot.windows.is_empty() {
            bail!("Claude subscription usage returned no quota windows");
        }
        Ok(snapshot)
    }
}

pub fn parse_claude(id: &str, data: &[u8], now: DateTime<Utc>) -> Result<Snapshot> {
    let mut snapshot = Snapshot {
        account_id: id.into(),
        observed_at: now,
        ..Snapshot::default()
    };
    let root: Value =
        serde_json::from_slice(data).map_err(|_| anyhow::anyhow!("Claude usage schema changed"))?;
    let available = root
        .get("rate_limits_available")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("Claude usage schema changed"))?;
    let limits = root
        .get("rate_limits")
        .ok_or_else(|| anyhow::anyhow!("Claude usage schema changed"))?;
    if !available {
        if !limits.is_null() {
            bail!("invalid unavailable Claude usage schema");
        }
        return Ok(snapshot);
    }
    if limits.is_null() {
        return Ok(snapshot);
    }
    let limits = limits
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Claude rate limits must be an object or null"))?;
    let mut keys: Vec<_> = limits.keys().collect();
    keys.sort();
    for key in keys {
        if key == "seven_day_breakdown"
            || key != "five_hour"
                && key != "seven_day"
                && !key.starts_with("five_hour_")
                && !key.starts_with("seven_day_")
        {
            continue;
        }
        let raw = &limits[key];
        if raw.is_null() {
            continue;
        }
        let window = parse_window(key, raw, "utilization")?;
        snapshot.windows.push(window);
    }
    for item in array_field(limits.get("model_scoped"))? {
        if is_fable(&item["display_name"]) {
            put_window(
                &mut snapshot,
                parse_window("seven_day_fable", item, "utilization")?,
            );
        }
    }
    for item in array_field(limits.get("limits"))? {
        let group = item["group"].as_str().unwrap_or_default();
        let name = match item["kind"].as_str() {
            Some("session") if group.is_empty() || group == "session" => "five_hour",
            Some("weekly_all") if group.is_empty() || group == "weekly" => "seven_day",
            Some("weekly_scoped")
                if (group.is_empty() || group == "weekly")
                    && is_fable(&item["scope"]["model"]["display_name"]) =>
            {
                "seven_day_fable"
            }
            _ => continue,
        };
        put_window(&mut snapshot, parse_window(name, item, "percent")?);
    }
    snapshot.available = true;
    snapshot.complete = true;
    Ok(snapshot)
}

fn array_field(value: Option<&Value>) -> Result<&[Value]> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => bail!("invalid Claude scoped limits"),
    }
}

fn is_fable(name: &Value) -> bool {
    let name = name
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let name = name.strip_prefix("claude ").unwrap_or(&name);
    name == "fable" || name.starts_with("fable ")
}

fn put_window(snapshot: &mut Snapshot, window: Window) {
    if let Some(existing) = snapshot
        .windows
        .iter_mut()
        .find(|w| w.name == window.name && w.model == window.model)
    {
        *existing = window;
    } else {
        snapshot.windows.push(window);
    }
}

fn parse_window(name: &str, item: &Value, percent: &str) -> Result<Window> {
    let used = item
        .get(percent)
        .context("Claude usage window fields missing")?;
    let reset = item
        .get("resets_at")
        .context("Claude usage window fields missing")?;
    let used: Option<f64> =
        serde_json::from_value(used.clone()).context("invalid Claude usage window")?;
    anyhow::ensure!(
        used.is_none_or(|v| (0.0..=100.0).contains(&v)),
        "Claude utilization outside 0..100"
    );
    let resets_at: Option<DateTime<Utc>> = serde_json::from_value(reset.clone())
        .map_err(|_| anyhow::anyhow!("Claude usage reset must be timezone-aware ISO timestamp"))?;
    Ok(Window {
        name: name.into(),
        limit_id: name.into(),
        model: name
            .strip_prefix("seven_day_")
            .or_else(|| name.strip_prefix("five_hour_"))
            .unwrap_or_default()
            .into(),
        duration: if name.starts_with("five_hour") {
            300
        } else {
            10080
        } * NANOS_PER_MINUTE,
        used,
        resets_at,
        ..Window::default()
    })
}
