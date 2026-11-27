use super::{Manager, process};
use crate::{
    config,
    model::{Account, Credential},
};
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::Path,
};

pub(super) struct NativeCredential {
    pub data: Vec<u8>,
    pub keychain: Option<(String, String)>,
}

impl Manager {
    pub(super) async fn read_credential(
        &self,
        a: &Account,
        p: &Path,
    ) -> Result<(Credential, bool)> {
        let native = native_credential_data(a, p).await?;
        parse_credential(&a.provider, &native.data, Utc::now())
    }
    pub(super) async fn refresh_claude_oauth(&self, a: &Account, profile: &Path) -> Result<()> {
        let native = native_credential_data(a, profile).await?;
        let mut root: Value = serde_json::from_slice(&native.data)
            .map_err(|_| anyhow::anyhow!("invalid native Claude credentials"))?;
        let oauth = root
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("native Claude OAuth credentials unavailable"))?;
        let refresh = oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Claude OAuth refresh token unavailable; sign in again")
            })?
            .to_owned();
        let mut request = json!({"grant_type":"refresh_token","refresh_token":refresh,"client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e"});
        if let Some(scopes) = oauth.get("scopes").and_then(Value::as_array)
            && !scopes.is_empty()
            && scopes.iter().all(Value::is_string)
        {
            request["scope"] = Value::String(
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        let resp = self
            .usage_client
            .post(&self.claude_token_url)
            .header("Accept", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Claude OAuth refresh endpoint unavailable"))?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            if status == reqwest::StatusCode::BAD_REQUEST
                || status == reqwest::StatusCode::UNAUTHORIZED
            {
                bail!("Claude OAuth refresh rejected; sign in again");
            }
            bail!("Claude OAuth refresh returned HTTP {}", status.as_u16());
        }
        let data = bounded_response(resp, 64 << 10)
            .await
            .map_err(|_| anyhow::anyhow!("invalid Claude OAuth refresh response size"))?;
        #[derive(serde::Deserialize)]
        struct Rotated {
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            expires_in: i64,
        }
        let rotated: Rotated = serde_json::from_slice(&data)
            .map_err(|_| anyhow::anyhow!("invalid Claude OAuth refresh response"))?;
        if rotated.access_token.is_empty() || !(1..=7 * 24 * 3600).contains(&rotated.expires_in) {
            bail!("invalid Claude OAuth refresh response");
        }
        oauth.insert("accessToken".into(), json!(rotated.access_token));
        oauth.insert(
            "refreshToken".into(),
            json!(if rotated.refresh_token.is_empty() {
                refresh
            } else {
                rotated.refresh_token
            }),
        );
        oauth.insert(
            "expiresAt".into(),
            json!((Utc::now() + Duration::seconds(rotated.expires_in)).timestamp_millis()),
        );
        let updated = serde_json::to_vec(&root)?;
        if let Some((service, user)) = native.keychain {
            update_keychain(a, profile, &service, &user, &updated).await
        } else {
            let profile = profile.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let target = profile.join(".credentials.json");
                if !fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_file()) {
                    bail!("Claude credential file is unavailable or unsafe");
                }
                config::write(target, &updated)
                    .map_err(|_| anyhow::anyhow!("cannot persist Claude credential rotation"))
            })
            .await?
        }
    }
}

pub(super) async fn bounded_response(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if resp.content_length().is_some_and(|size| size > max as u64) {
        bail!("response too large");
    }
    let mut data = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if data.len() + chunk.len() > max {
            bail!("response too large");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

pub(super) async fn native_credential_data(a: &Account, p: &Path) -> Result<NativeCredential> {
    #[cfg(target_os = "macos")]
    if a.provider == "claude" {
        use sha2::{Digest, Sha256};
        let mut id = tokio::process::Command::new("/usr/bin/id");
        id.arg("-un")
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        process::configure(&mut id);
        if let Ok(Ok(bytes)) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            process::bounded_output(id, 4096),
        )
        .await
        {
            let user = String::from_utf8_lossy(&bytes).trim().to_owned();
            if !user.is_empty() {
                let service = format!(
                    "Claude Code-credentials-{}",
                    &format!("{:x}", Sha256::digest(p.to_string_lossy().as_bytes()))[..8]
                );
                let mut cmd = tokio::process::Command::new("/usr/bin/security");
                cmd.args(["find-generic-password", "-a", &user, "-w", "-s", &service])
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                process::configure(&mut cmd);
                if let Ok(Ok(data)) = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    process::bounded_output(cmd, 1 << 20),
                )
                .await
                    && !data.is_empty()
                {
                    return Ok(NativeCredential {
                        data,
                        keychain: Some((service, user)),
                    });
                }
            }
        }
    }
    let path = p.join(if a.provider == "claude" {
        ".credentials.json"
    } else {
        "auth.json"
    });
    let data = tokio::task::spawn_blocking(move || read_native_file(&path)).await??;
    Ok(NativeCredential {
        data,
        keychain: None,
    })
}

pub(super) fn read_native_file(path: &Path) -> Result<Vec<u8>> {
    if !fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file()) {
        bail!("native credentials unavailable; run account login");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let f = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("cannot read native credential file"))?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        bail!("native credentials unavailable; run account login");
    }
    if meta.len() > 1 << 20 {
        bail!("native credentials file is too large");
    }
    let mut data = Vec::new();
    f.take((1 << 20) + 1)
        .read_to_end(&mut data)
        .map_err(|_| anyhow::anyhow!("cannot read native credential file"))?;
    if data.len() > 1 << 20 {
        bail!("native credentials file is too large");
    }
    Ok(data)
}

pub(super) fn parse_credential(
    provider: &str,
    data: &[u8],
    now: DateTime<Utc>,
) -> Result<(Credential, bool)> {
    let root: Value =
        serde_json::from_slice(data).map_err(|_| anyhow::anyhow!("invalid native credentials"))?;
    if !root.is_object() {
        bail!("invalid native credentials");
    }
    let mut credential = Credential::default();
    let mut expiry = None;
    if provider == "codex" {
        credential.token = root
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into();
        credential.account_id = root
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into();
        if let Some(claims) = jwt_claims(&credential.token)
            && let Some(exp) = claims
                .get("exp")
                .and_then(Value::as_i64)
                .filter(|exp| *exp > 0)
        {
            expiry = DateTime::from_timestamp(exp, 0);
        }
    } else {
        let oauth = root
            .get("claudeAiOauth")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!("native Claude OAuth credentials unavailable; run account login")
            })?;
        credential.token = oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into();
        if let Some(exp) = oauth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .filter(|exp| *exp > 0)
        {
            expiry = DateTime::from_timestamp_millis(exp);
        }
    }
    if credential.token.is_empty() {
        bail!("native account has no access token; run account login");
    }
    Ok((
        credential,
        expiry.is_some_and(|exp| exp <= now + Duration::minutes(1)),
    ))
}
pub(super) fn jwt_claims(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    parts.next()?;
    let body = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()
}

async fn update_keychain(
    a: &Account,
    profile: &Path,
    service: &str,
    user: &str,
    updated: &[u8],
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    // Secrets go only over stdin. security's interpreter reports success even on
    // some failed commands, so persistence must be verified by reading back.
    let hex: String = updated.iter().map(|b| format!("{b:02x}")).collect();
    let command = format!(
        "add-generic-password -U -a {} -s {} -X {hex}\n",
        serde_json::to_string(user)?,
        serde_json::to_string(service)?
    );
    let mut cmd = tokio::process::Command::new("security");
    cmd.arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    process::configure(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|_| anyhow::anyhow!("cannot update native Claude Keychain credential"))?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("cannot update native Claude Keychain credential"))?;
    input
        .write_all(command.as_bytes())
        .await
        .map_err(|_| anyhow::anyhow!("cannot update native Claude Keychain credential"))?;
    drop(input);
    if !child
        .wait()
        .await
        .map_err(|_| anyhow::anyhow!("cannot update native Claude Keychain credential"))?
        .success()
    {
        bail!("cannot update native Claude Keychain credential");
    }
    let actual = native_credential_data(a, profile).await?;
    if actual
        .keychain
        .as_ref()
        .is_none_or(|(s, u)| s != service || u != user)
        || actual.data.trim_ascii() != updated
    {
        bail!("native Claude Keychain credential update was not verified");
    }
    Ok(())
}
