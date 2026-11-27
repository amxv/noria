use super::{
    Manager,
    credentials::{jwt_claims, read_native_file},
    process::{bounded_output, command},
};
use crate::model::Account;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Identity {
    pub email: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    pub authenticated: bool,
}
impl Manager {
    pub async fn identity(&self, a: &Account) -> Result<Identity> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let p = self.profile(a).await?;
            if a.provider == "codex" {
                let path = p.join("auth.json");
                let bytes = tokio::task::spawn_blocking(move || read_native_file(&path))
                    .await?
                    .map_err(|_| anyhow::anyhow!("Codex profile is not signed in"))?;
                let root: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("Codex profile is not signed in"))?;
                let access = root
                    .pointer("/tokens/access_token")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if access.is_empty() {
                    bail!("Codex profile is not signed in");
                }
                let mut email = display_email_from_jwt(
                    root.pointer("/tokens/id_token")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                if email.is_empty() {
                    email = display_email_from_jwt(access);
                }
                return Ok(Identity {
                    email: valid_email(&email),
                    account_id: root
                        .pointer("/tokens/account_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    authenticated: true,
                });
            }
            let cmd = command(
                &self.claude_command,
                &p,
                "claude",
                &["auth", "status", "--json"],
            );
            let bytes = bounded_output(cmd, 1 << 20)
                .await
                .map_err(|_| anyhow::anyhow!("Claude profile is not signed in"))?;
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct NativeIdentity {
                #[serde(default)]
                logged_in: bool,
                #[serde(default)]
                email: String,
                #[serde(default)]
                account_uuid: String,
            }
            let identity: NativeIdentity = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("Claude identity response changed"))?;
            Ok(Identity {
                email: valid_email(&identity.email),
                account_id: identity.account_uuid,
                authenticated: identity.logged_in,
            })
        })
        .await
        .map_err(|_| anyhow::anyhow!("native identity deadline exceeded"))?
    }
}
pub(super) fn display_email_from_jwt(token: &str) -> String {
    let Some(claims) = jwt_claims(token) else {
        return String::new();
    };
    if let Some(email) = claims.get("email").and_then(Value::as_str) {
        return email.into();
    }
    for key in ["https://api.openai.com/profile", "profile"] {
        if let Some(email) = claims
            .get(key)
            .and_then(|v| v.get("email"))
            .and_then(Value::as_str)
        {
            return email.into();
        }
    }
    String::new()
}
pub(super) fn valid_email(value: &str) -> String {
    if value.len() > 254 || value.contains(['\r', '\n', '\0', '\t']) {
        return String::new();
    }
    let Some((local, domain)) = value.rsplit_once('@') else {
        return String::new();
    };
    if local.is_empty()
        || domain.is_empty()
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || local.contains('@')
    {
        return String::new();
    }
    // Match Go net/mail's bare addr-spec acceptance, including RFC 6532
    // international addresses. Display names, quoting and comments change
    // ParseAddress.Address and therefore fail the original equality check.
    let atom = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.ends_with('.')
            && !part.contains("..")
            && part.chars().all(|c| {
                (c >= '\u{80}' || ('!'..='~').contains(&c))
                    && !matches!(
                        c,
                        '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '"'
                    )
            })
    };
    let domain_valid =
        if let Some(literal) = domain.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if let Some(ip) = literal.strip_prefix("IPv6:") {
                ip.parse::<std::net::IpAddr>().is_ok()
            } else {
                literal
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| match ip {
                        std::net::IpAddr::V4(_) => true,
                        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some(),
                    })
            }
        } else {
            atom(domain)
        };
    if !atom(local) || !domain_valid {
        return String::new();
    }
    value.into()
}
