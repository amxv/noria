use super::RequestMeta;
use crate::model::{Account, Credential};
use anyhow::{Result, bail};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::net::IpAddr;

pub(super) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}
pub(super) fn local_host(value: &str) -> bool {
    let host = if value.starts_with('[') {
        let Some((host, tail)) = value.split_once(']') else {
            return false;
        };
        if !tail.is_empty() && (!tail.starts_with(':') || tail[1..].contains(':')) {
            return false;
        }
        &host[1..]
    } else if value.matches(':').count() == 1 {
        value.split_once(':').map(|(v, _)| v).unwrap_or(value)
    } else {
        value
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| {
            ip.is_loopback()
                || matches!(ip, IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some_and(|mapped| mapped.is_loopback()))
        })
}
pub(super) fn valid_origin(headers: &HeaderMap) -> bool {
    let value = header(headers, "origin");
    if value.is_empty() {
        return true;
    }
    let Some((scheme, authority)) = value.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https")
        && !authority.contains(['/', '?', '#', '@'])
        && authority.eq_ignore_ascii_case(header(headers, "host"))
        && local_host(authority)
}
pub(super) fn route(path: &str) -> Option<(&'static str, &'static str)> {
    if let Some(native) = path.strip_prefix("/codex/") {
        return route(&format!("/{native}")).filter(|(provider, _)| *provider == "codex");
    }
    if let Some(native) = path.strip_prefix("/claude/") {
        return route(&format!("/{native}")).filter(|(provider, _)| *provider == "claude");
    }
    match path {
        "/responses" | "/v1/responses" | "/backend-api/codex/responses" => {
            Some(("codex", "/responses"))
        }
        "/responses/compact" | "/v1/responses/compact" | "/backend-api/codex/responses/compact" => {
            Some(("codex", "/responses/compact"))
        }
        "/messages" | "/v1/messages" => Some(("claude", "/v1/messages")),
        "/v1/messages/count_tokens" => Some(("claude", "/v1/messages/count_tokens")),
        _ => None,
    }
}
pub(super) fn raw_string(raw: Option<&RawValue>) -> String {
    raw.and_then(|raw| serde_json::from_str(raw.get()).ok())
        .unwrap_or_default()
}
pub(super) fn session_key(headers: &HeaderMap, object: &RequestMeta<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (name, key) in [
        ("x-claude-code-session-id", "X-Claude-Code-Session-Id"),
        ("x-codex-session-id", "X-Codex-Session-Id"),
        ("session-id", "Session-Id"),
        ("session_id", "Session_id"),
    ] {
        let value = header(headers, name);
        if !value.is_empty() {
            parts.extend([key.into(), value.into()]);
            break;
        }
    }
    if parts.is_empty() {
        for (key, raw) in [
            ("session_id", object.session_id),
            ("prompt_cache_key", object.prompt_cache_key),
        ] {
            let value = raw_string(raw);
            if !value.is_empty() {
                parts.extend([key.into(), value]);
                break;
            }
        }
    }
    if parts.is_empty() {
        #[derive(serde::Deserialize)]
        struct Metadata<'a> {
            #[serde(borrow)]
            user_id: Option<&'a RawValue>,
        }
        if let Some(metadata) = object
            .metadata
            .and_then(|raw| serde_json::from_str::<Metadata<'_>>(raw.get()).ok())
        {
            let user = raw_string(metadata.user_id);
            if let Ok(identity) =
                serde_json::from_str::<std::collections::HashMap<String, String>>(&user)
                && let Some(session) = identity.get("session_id").filter(|s| !s.is_empty())
            {
                parts.extend(["metadata.session_id".into(), session.clone()]);
            }
        }
    }
    if parts.is_empty() && !header(headers, "x-noria-session").is_empty() {
        parts.extend(["wrapper".into(), header(headers, "x-noria-session").into()]);
    }
    if !header(headers, "x-claude-code-agent-id").is_empty() {
        parts.extend([
            "agent".into(),
            header(headers, "x-claude-code-agent-id").into(),
        ]);
    }
    if parts.is_empty() {
        return String::new();
    }
    // Go's encoding/json escapes HTML and JavaScript line separators even for
    // this string array. Preserve its exact bytes so persisted affinities also
    // survive migration for native session metadata containing those characters.
    let encoded = serde_json::to_string(&parts)
        .expect("strings serialize")
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Sha256::digest(encoded.as_bytes())
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
pub(super) fn endpoint(account: &Account, path: &str, query: &str) -> Result<url::Url> {
    let default = if account.provider == "codex" {
        "https://chatgpt.com/backend-api/codex"
    } else {
        "https://api.anthropic.com"
    };
    let mut url = url::Url::parse(if account.endpoint.is_empty() {
        default
    } else {
        &account.endpoint
    })?;
    if url.host_str().is_none()
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("invalid account endpoint");
    }
    if url.scheme() != "https" && !local_host(url.host_str().unwrap_or_default()) {
        bail!("non-loopback account endpoints must use HTTPS");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("account endpoint cannot contain a query or fragment");
    }
    url.set_path(&format!(
        "{}{path}",
        url.path().strip_suffix('/').unwrap_or(url.path())
    ));
    if !query.is_empty() {
        url.set_query(Some(query));
    }
    Ok(url)
}
pub(super) fn strip_hop_headers(headers: &mut HeaderMap) {
    let connection_names: Vec<HeaderName> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| HeaderName::from_bytes(v.trim().as_bytes()).ok())
        .collect();
    for name in connection_names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
pub(super) fn upstream_headers(
    original: &HeaderMap,
    provider: &str,
    credential: &Credential,
) -> Result<HeaderMap> {
    let mut headers = original.clone();
    strip_hop_headers(&mut headers);
    for name in [
        "authorization",
        "x-api-key",
        "proxy-authorization",
        "cookie",
        "chatgpt-account-id",
        "x-noria-session",
        "origin",
        "referer",
        "host",
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-extensions",
        "sec-websocket-protocol",
    ] {
        headers.remove(name);
    }
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", credential.token))?,
    );
    if provider == "codex" && !credential.account_id.is_empty() {
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(&credential.account_id)?,
        );
    }
    if provider == "claude" {
        let mut betas = headers
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        if !betas.split(',').any(|v| v.trim() == "oauth-2025-04-20") {
            if !betas.trim().is_empty() {
                betas.push(',');
            }
            betas.push_str("oauth-2025-04-20");
        }
        headers.insert("anthropic-beta", HeaderValue::from_str(&betas)?);
    }
    Ok(headers)
}
pub(super) fn response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut headers = headers.clone();
    strip_hop_headers(&mut headers);
    for name in [
        "set-cookie",
        "authorization",
        "x-api-key",
        "chatgpt-account-id",
    ] {
        headers.remove(name);
    }
    headers
}
