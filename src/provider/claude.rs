use super::{
    Manager,
    credentials::bounded_response,
    process::{NativeProcess, command},
};
use crate::{
    core::NANOS_PER_MINUTE,
    model::{Account, Snapshot, Window},
};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::path::Path;

impl Manager {
    pub(super) async fn claude_quota(&self, a: &Account, p: &Path) -> Result<Snapshot> {
        let started = Utc::now();
        let snapshot = self.claude_control_usage(a, p, started).await?;
        if !snapshot.windows.is_empty() {
            return Ok(snapshot);
        }
        let Ok((credential, expired)) = self.read_credential(a, p).await else {
            return Ok(snapshot);
        };
        if expired {
            bail!("Claude OAuth credential expired; sign in again");
        }
        self.claude_oauth_usage(&a.id, &credential.token, started)
            .await
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
                bail!("Claude subscription usage authorization failed; sign in again");
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
        let fields = raw
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Claude usage window schema changed"))?;
        let used = fields
            .get("utilization")
            .ok_or_else(|| anyhow::anyhow!("Claude usage window fields missing"))?;
        let reset = fields
            .get("resets_at")
            .ok_or_else(|| anyhow::anyhow!("Claude usage window fields missing"))?;
        let used: Option<f64> = serde_json::from_value(used.clone())
            .map_err(|_| anyhow::anyhow!("invalid Claude usage window"))?;
        let reset: Option<String> = serde_json::from_value(reset.clone())
            .map_err(|_| anyhow::anyhow!("invalid Claude usage window"))?;
        if used.is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v)) {
            bail!("Claude utilization outside 0..100");
        }
        let duration = if key.starts_with("five_hour") {
            300 * NANOS_PER_MINUTE
        } else {
            10080 * NANOS_PER_MINUTE
        };
        let model = key
            .strip_prefix("seven_day_")
            .or_else(|| key.strip_prefix("five_hour_"))
            .unwrap_or_default()
            .to_owned();
        let resets_at = reset
            .map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|t| t.with_timezone(&Utc))
                    .map_err(|_| {
                        anyhow::anyhow!("Claude usage reset must be timezone-aware ISO timestamp")
                    })
            })
            .transpose()?;
        snapshot.windows.push(Window {
            name: key.clone(),
            limit_id: key.clone(),
            model,
            used,
            duration,
            resets_at,
            ..Window::default()
        });
    }
    snapshot.available = true;
    Ok(snapshot)
}
