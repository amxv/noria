use super::{
    Manager,
    process::{NativeProcess, command, run_silent},
};
use crate::model::Account;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};

impl Manager {
    pub async fn browser_login(&self, a: &Account) -> Result<()> {
        self.login(a).await
    }
    pub async fn login(&self, a: &Account) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5 * 60), async {
            let p = self.profile(a).await?;
            let _guard = self.lock_profile(&p).await?;
            if a.provider == "codex" {
                self.codex_login(&p).await
            } else {
                let cmd = command(
                    &self.claude_command,
                    &p,
                    "claude",
                    &["auth", "login", "--claudeai"],
                );
                run_silent(cmd).await.map_err(|_| {
                    anyhow::anyhow!(
                        "Claude sign-in failed; retry or use the native CLI in a terminal"
                    )
                })
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("native sign-in cancelled or timed out"))?
    }
    async fn codex_login(&self, p: &Path) -> Result<()> {
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
            ],
        );
        let mut process = NativeProcess::spawn(cmd, 1 << 20)
            .map_err(|_| anyhow::anyhow!("install Codex before signing in"))?;
        process.write(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"noria","version":env!("CARGO_PKG_VERSION")}}})).await?;
        let mut login_id = String::new();
        loop {
            let msg = process.read().await?;
            if msg.get("error").is_some_and(|v| !v.is_null()) {
                bail!("Codex login was rejected");
            }
            match msg.get("id").and_then(Value::as_u64) {
                Some(1) => {
                    process.write(&json!({"method":"initialized"})).await?;
                    process.write(&json!({"id":2,"method":"account/login/start","params":{"type":"chatgpt"}})).await?;
                }
                Some(2) => {
                    let raw = msg
                        .pointer("/result/authUrl")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let id = msg
                        .pointer("/result/loginId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    validate_login_url(raw, id)?;
                    login_id = id.into();
                    self.open_login_page(raw).await.map_err(|_| {
                        anyhow::anyhow!(
                            "could not open the provider sign-in page in your default browser"
                        )
                    })?;
                }
                _ => {}
            }
            if msg.get("method").and_then(Value::as_str) == Some("account/login/completed") {
                if login_id.is_empty()
                    || msg.pointer("/params/loginId").and_then(Value::as_str) != Some(&login_id)
                {
                    continue;
                }
                if msg.pointer("/params/success").and_then(Value::as_bool) != Some(true) {
                    bail!("Codex sign-in did not complete");
                }
                return Ok(());
            }
        }
    }
    async fn open_login_page(&self, url: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(open) = self.login_opener {
            return open(url);
        }
        open_login_url(url).await
    }
}
pub(super) fn validate_login_url(raw: &str, id: &str) -> Result<()> {
    let u = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("unexpected provider login URL"))?;
    // URL normalizes an explicit :443 to no port. Check the raw authority too.
    let authority = raw
        .split_once("://")
        .map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    if u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || authority.contains(':')
        || !matches!(u.host_str(), Some("auth.openai.com" | "chatgpt.com"))
        || id.is_empty()
    {
        bail!("unexpected provider login URL");
    }
    Ok(())
}
async fn open_login_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if !cmd.status().await?.success() {
        bail!("browser launch failed");
    }
    Ok(())
}
