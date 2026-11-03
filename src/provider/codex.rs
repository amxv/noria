use super::{
    Manager,
    process::{NativeProcess, command},
};
use crate::{
    core::NANOS_PER_MINUTE,
    model::{Account, Snapshot, Window},
};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

impl Manager {
    pub(super) async fn codex_control(
        &self,
        a: &Account,
        p: &Path,
        refresh_token: bool,
    ) -> Result<Snapshot> {
        let started = Utc::now();
        let cmd = command(
            &self.codex_command,
            p,
            "codex",
            &[
                "app-server",
                "-c",
                r#"cli_auth_credentials_store="file""#,
                "-c",
                r#"model_provider="openai""#,
                "-c",
                r#"openai_base_url="https://api.openai.com/v1""#,
            ],
        );
        let mut process = NativeProcess::spawn(cmd, 4 << 20)
            .map_err(|_| anyhow::anyhow!("cannot start codex app-server; install Codex CLI"))?;
        rpc(&mut process,1,"initialize",json!({"clientInfo":{"name":"noria","title":"Noria","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
        process
            .write(&json!({"method":"initialized","params":{}}))
            .await?;
        let account = rpc(
            &mut process,
            2,
            "account/read",
            json!({"refreshToken":refresh_token}),
        )
        .await?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Auth {
            #[serde(default)]
            account: Value,
            #[serde(default)]
            requires_openai_auth: bool,
        }
        let auth: Auth = serde_json::from_value(account)
            .map_err(|_| anyhow::anyhow!("invalid Codex account response"))?;
        if auth.requires_openai_auth && auth.account.is_null() {
            return Ok(Snapshot {
                account_id: a.id.clone(),
                observed_at: started,
                auth_invalid: true,
                ..Snapshot::default()
            });
        }
        let raw = rpc(&mut process, 3, "account/rateLimits/read", json!({})).await?;
        parse_codex(&a.id, &serde_json::to_vec(&raw)?, started)
    }
}
async fn rpc(process: &mut NativeProcess, id: u32, method: &str, params: Value) -> Result<Value> {
    process
        .write(&json!({"id":id,"method":method,"params":params}))
        .await?;
    loop {
        let msg = process.read().await?;
        if msg.get("id").and_then(Value::as_u64) != Some(id as u64) {
            continue;
        }
        if msg.get("error").is_some_and(|v| !v.is_null()) {
            bail!("Codex {method} control failed; verify account login");
        }
        return msg
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Codex control result missing"));
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    #[serde(default)]
    primary: Value,
    #[serde(default)]
    secondary: Value,
    #[serde(default)]
    limit_name: String,
    #[serde(default)]
    ordinary_usage_allowed: Option<bool>,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
    #[serde(default)]
    normal_model_slug: String,
    #[serde(default)]
    spend_control_reached: Option<bool>,
}

pub fn parse_codex(id: &str, data: &[u8], now: DateTime<Utc>) -> Result<Snapshot> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Root {
        #[serde(default)]
        rate_limits: Value,
        #[serde(default)]
        rate_limits_by_limit_id: Option<std::collections::BTreeMap<String, Value>>,
        #[serde(default)]
        ordinary_usage_allowed: Option<bool>,
    }
    let root: Root =
        serde_json::from_slice(data).map_err(|_| anyhow::anyhow!("invalid Codex quota schema"))?;
    let by_id = root.rate_limits_by_limit_id.unwrap_or_default();
    let raw = by_id
        .get("codex")
        .filter(|v| !v.is_null())
        .unwrap_or(&root.rate_limits);
    if raw.is_null() {
        bail!("Codex response contains no canonical quota bucket");
    }
    let mut s = Snapshot {
        account_id: id.into(),
        observed_at: now,
        ..Snapshot::default()
    };
    append_bucket(&mut s, raw, "", "codex", root.ordinary_usage_allowed)?;
    for (key, raw) in &by_id {
        if !(key.starts_with("codex_") || key.starts_with("codex-")) || raw.is_null() {
            continue;
        }
        let b: Bucket = serde_json::from_value(raw.clone())
            .map_err(|_| anyhow::anyhow!("invalid scoped Codex quota bucket"))?;
        let scope = if b.normal_model_slug.is_empty() {
            b.limit_name
        } else {
            b.normal_model_slug
        };
        if scope.is_empty() {
            continue;
        }
        append_bucket(&mut s, raw, &scope, key, None)?;
    }
    s.available = true;
    Ok(s)
}
fn append_bucket(
    s: &mut Snapshot,
    raw: &Value,
    scope: &str,
    limit_id: &str,
    ordinary: Option<bool>,
) -> Result<()> {
    let mut b: Bucket = serde_json::from_value(raw.clone())
        .map_err(|_| anyhow::anyhow!("invalid Codex quota bucket"))?;
    if scope.is_empty() && ordinary.is_some() {
        b.ordinary_usage_allowed = ordinary;
    }
    let blocked = b.ordinary_usage_allowed == Some(false)
        || b.ordinary_usage_allowed.is_none()
            && b.rate_limit_reached_type
                .as_ref()
                .is_some_and(|r| !r.is_empty())
        || b.spend_control_reached == Some(true);
    let gate_allowed = b.ordinary_usage_allowed.or_else(|| {
        (raw.get("rateLimitReachedType").is_some() || b.spend_control_reached == Some(true))
            .then_some(!blocked)
    });
    let mut blocked_until = None;
    for (name, raw) in [("primary", b.primary), ("secondary", b.secondary)] {
        if raw.is_null() {
            continue;
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawWindow {
            #[serde(default)]
            used_percent: Option<f64>,
            #[serde(default)]
            window_duration_mins: Option<i64>,
            #[serde(default)]
            resets_at: Option<i64>,
        }
        let w: RawWindow = serde_json::from_value(raw)
            .map_err(|_| anyhow::anyhow!("invalid Codex quota window"))?;
        if w.used_percent
            .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
        {
            bail!("Codex utilization outside 0..100");
        }
        let mut out = Window {
            name: name.into(),
            limit_id: limit_id.into(),
            model: scope.into(),
            used: w.used_percent,
            ..Window::default()
        };
        if b.ordinary_usage_allowed == Some(true) {
            out.allowed = Some(true);
        }
        if let Some(mins) = w.window_duration_mins {
            if !(1..=525600).contains(&mins) {
                bail!("invalid Codex quota duration");
            }
            out.duration = mins * NANOS_PER_MINUTE;
            if mins == 300 {
                out.name = "five_hour".into();
            } else if mins == 10080 {
                out.name = "seven_day".into();
            }
        }
        if let Some(reset) = w.resets_at {
            if reset < 0 {
                bail!("invalid Codex quota reset");
            }
            let at = DateTime::from_timestamp(reset, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid Codex quota reset"))?;
            out.resets_at = Some(at);
            if blocked
                && w.used_percent.is_some_and(|v| v >= 100.0)
                && blocked_until.is_none_or(|old| at > old)
            {
                blocked_until = Some(at);
            }
        }
        s.windows.push(out);
    }
    if gate_allowed.is_some() || blocked {
        if b.spend_control_reached == Some(true)
            || b.rate_limit_reached_type
                .is_some_and(|r| r.starts_with("workspace_"))
        {
            blocked_until = None;
        }
        s.windows.push(Window {
            name: "provider".into(),
            limit_id: limit_id.into(),
            model: scope.into(),
            exhausted: blocked,
            allowed: gate_allowed,
            resets_at: blocked_until,
            ..Window::default()
        });
    }
    Ok(())
}
