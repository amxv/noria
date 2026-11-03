use super::*;
use crate::core::{Scheduler, UnavailableError};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use std::collections::HashSet;

#[test]
fn codex_reversed_null_and_scoped_limits() {
    let s=parse_codex("a",br#"{"rateLimits":{"primary":{"usedPercent":99}},"rateLimitsByLimitId":{"chatgpt":{"primary":{"usedPercent":0}},"codex":{"primary":{"usedPercent":75,"windowDurationMins":10080,"resetsAt":2000000000},"secondary":{"usedPercent":null,"windowDurationMins":300,"resetsAt":null}},"codex_model":{"limitName":"GPT-5.3-Codex","primary":{"usedPercent":100,"windowDurationMins":10080},"ordinaryUsageAllowed":true},"codex_ignored":{"primary":{"usedPercent":100}},"other":{"limitName":"other","primary":{"usedPercent":100}}}}"#,Utc::now()).unwrap();
    assert_eq!(s.windows.len(), 4);
    assert_eq!(s.windows[0].name, "seven_day");
    assert_eq!(s.windows[0].used, Some(75.0));
    assert_eq!(s.windows[0].resets_at.unwrap().timestamp(), 2000000000);
    assert_eq!(s.windows[1].name, "five_hour");
    assert_eq!(s.windows[1].used, None);
    assert_eq!(s.windows[1].resets_at, None);
    assert_eq!(s.windows[2].model, "GPT-5.3-Codex");
    assert_eq!(s.windows[2].allowed, Some(true));
    assert!(!s.windows[2].exhausted);
}
#[test]
fn codex_malformed_and_backend_block() {
    for raw in [
        r#"{"rateLimitsByLimitId":{"chatgpt":{}}}"#,
        r#"{"rateLimits":{"primary":{"usedPercent":101}}}"#,
        r#"{"rateLimits":{"primary":{"resetsAt":"bad"}}}"#,
        r#"{"rateLimits":{"primary":{"windowDurationMins":0}}}"#,
    ] {
        assert!(parse_codex("a", raw.as_bytes(), Utc::now()).is_err());
    }
    let s = parse_codex(
        "a",
        br#"{"rateLimits":{"primary":null,"secondary":null,"ordinaryUsageAllowed":false}}"#,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(s.windows.len(), 1);
    assert!(s.windows[0].exhausted);
    let s = parse_codex(
        "a",
        br#"{"rateLimits":{"primary":{"usedPercent":0},"secondary":{"usedPercent":null}}}"#,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(s.windows[0].used, Some(0.0));
    assert_eq!(s.windows[1].used, None);
}
#[test]
fn bucket_gate_does_not_exhaust_healthy_week_and_recovers_on_explicit_null() {
    let t = Utc::now();
    let reset = (t + ChronoDuration::hours(1)).timestamp();
    let week = (t + ChronoDuration::days(5)).timestamp();
    let raw = json!({"rateLimits":{"primary":{"usedPercent":100,"windowDurationMins":300,"resetsAt":reset},"secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":week},"rateLimitReachedType":"rate_limit_reached"}});
    let snapshot = parse_codex("a", &serde_json::to_vec(&raw).unwrap(), t).unwrap();
    let scheduler = Scheduler::new(
        "",
        vec![Account {
            id: "a".into(),
            provider: "codex".into(),
            enabled: true,
            ..Account::default()
        }],
    )
    .unwrap();
    scheduler.update(snapshot).unwrap();
    let error = scheduler
        .acquire("codex", "test", "model", &HashSet::new())
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<UnavailableError>()
            .unwrap()
            .retry_at
            .unwrap()
            .timestamp(),
        reset
    );
    let raw = json!({"rateLimits":{"primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":reset},"secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":week},"rateLimitReachedType":null}});
    scheduler
        .update(
            parse_codex(
                "a",
                &serde_json::to_vec(&raw).unwrap(),
                t + ChronoDuration::seconds(1),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        scheduler
            .acquire("codex", "test", "model", &HashSet::new())
            .is_ok()
    );
}
#[test]
fn credit_gate_model_identity_and_distinct_limit_ids() {
    let s=parse_codex("a",&serde_json::to_vec(&json!({"rateLimits":{"primary":{"usedPercent":100,"windowDurationMins":300,"resetsAt":(Utc::now()+ChronoDuration::hours(1)).timestamp()},"rateLimitReachedType":"workspace_owner_credits_depleted"}})).unwrap(),Utc::now()).unwrap();
    let gate = s.windows.iter().find(|w| w.name == "provider").unwrap();
    assert!(gate.exhausted);
    assert_eq!(gate.resets_at, None);
    let s=parse_codex("a",br#"{"rateLimits":{},"rateLimitsByLimitId":{"codex_one":{"limitName":"Human label","normalModelSlug":"gpt-test-model","primary":{"usedPercent":10,"windowDurationMins":300}},"codex_two":{"limitName":"Human label","normalModelSlug":"gpt-test-model","primary":{"usedPercent":20,"windowDurationMins":300}}}}"#,Utc::now()).unwrap();
    assert_eq!(s.windows.len(), 2);
    assert_eq!(s.windows[0].model, "gpt-test-model");
    assert_ne!(s.windows[0].limit_id, s.windows[1].limit_id);
}
#[test]
fn claude_partial_scopes_offset_and_schema_validation() {
    let s=parse_claude("a",br#"{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":12.5,"resets_at":"2026-10-20T01:02:03+05:30"},"seven_day":null,"seven_day_sonnet":{"utilization":null,"resets_at":null},"seven_day_breakdown":{"rows":[]},"extra_usage":{"is_enabled":false}}}"#,Utc::now()).unwrap();
    assert!(s.available);
    assert_eq!(s.windows.len(), 2);
    assert_eq!(s.windows[0].used, Some(12.5));
    assert_eq!(s.windows[1].model, "sonnet");
    assert_eq!(s.windows[1].used, None);
    assert_eq!(
        s.windows[0].resets_at.unwrap().to_rfc3339(),
        "2026-10-19T19:32:03+00:00"
    );
    for raw in [
        r#"{"rate_limits_available":false,"rate_limits":null}"#,
        r#"{"rate_limits_available":true,"rate_limits":null}"#,
    ] {
        let s = parse_claude("a", raw.as_bytes(), Utc::now()).unwrap();
        assert!(!s.available && !s.auth_invalid && s.windows.is_empty());
    }
    for raw in [
        r#"{}"#,
        r#"{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":101,"resets_at":null}}}"#,
        r#"{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":2,"resets_at":"2026-10-20T00:00:00"}}}"#,
    ] {
        assert!(parse_claude("a", raw.as_bytes(), Utc::now()).is_err());
    }
}
#[test]
fn expiry_identity_and_login_url_validation() {
    use base64::Engine;
    let jwt = format!(
        "e.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"exp":1,"email":"user@example.com"}"#)
    );
    let (c, expired) = credentials::parse_credential(
        "codex",
        &serde_json::to_vec(&json!({"tokens":{"access_token":jwt,"account_id":"fixed"}})).unwrap(),
        Utc::now(),
    )
    .unwrap();
    assert!(expired);
    assert_eq!(c.account_id, "fixed");
    assert_eq!(
        identity::display_email_from_jwt(&c.token),
        "user@example.com"
    );
    for email in [
        "hello\n@x.com",
        "Name <u@x.com>",
        "a\t@x.com",
        "bad email",
        "u@x.com\0",
    ] {
        assert!(identity::valid_email(email).is_empty());
    }
    assert_eq!(
        identity::valid_email("user+work@example.com"),
        "user+work@example.com"
    );
    assert_eq!(identity::valid_email("用户@例子.测试"), "用户@例子.测试");
    assert_eq!(
        identity::valid_email("user@[127.0.0.1]"),
        "user@[127.0.0.1]"
    );
    for url in [
        "http://auth.openai.com/x",
        "https://auth.openai.com:443/x",
        "https://evil.example/x",
        "https://auth.openai.com@evil.example/x",
        "https://user@auth.openai.com/x",
    ] {
        assert!(login::validate_login_url(url, "id").is_err());
    }
    assert!(login::validate_login_url("https://auth.openai.com/oauth?code=fixture", "id").is_ok());
    assert!(login::validate_login_url("https://chatgpt.com/auth", "id").is_ok());
}

fn fixture(provider: &str) -> (tempfile::TempDir, Manager, Account) {
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profiles/fixture");
    config::private_dir(&profile).unwrap();
    let manager = Manager::new(temp.path());
    let account = Account {
        id: "fixture".into(),
        provider: provider.into(),
        profile: profile.to_string_lossy().into(),
        enabled: true,
        ..Account::default()
    };
    (temp, manager, account)
}
#[tokio::test]
async fn valid_credentials_bypass_held_quota_lock_and_reject_external_profiles() {
    let (temp, manager, mut account) = fixture("codex");
    let p = Path::new(&account.profile);
    config::write(
        p.join("auth.json"),
        br#"{"tokens":{"access_token":"valid-fixture","account_id":"fixture"}}"#,
    )
    .unwrap();
    let _guard = config::lock(p, "native").unwrap();
    let credential = tokio::time::timeout(Duration::from_secs(2), manager.credentials(&account))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(credential.token, "valid-fixture");
    let outside = tempfile::tempdir().unwrap();
    account.profile = outside.path().to_string_lossy().into();
    assert!(manager.credentials(&account).await.is_err());
    account.profile = temp.path().to_string_lossy().into();
    assert!(manager.credentials(&account).await.is_err());
}

#[cfg(unix)]
fn script(temp: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = temp.join(name);
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    p
}
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_codex_refresh_serializes_across_managers_and_passive_read_does_not_rotate() {
    let (temp, mut manager, a) = fixture("codex");
    let executable = script(
        temp.path(),
        "fake-codex",
        r##"#!/usr/bin/env python3
import json,os,sys
p=os.environ['CODEX_HOME']
assert not any(k.startswith(('OPENAI_','ANTHROPIC_','NORIA_','NODE_')) for k in os.environ)
with open(p+'/calls','a') as calls:
 for line in sys.stdin:
  r=json.loads(line); method=r['method']; result={}
  calls.write(method+'\n');calls.flush()
  if method=='initialize': assert r['params']['capabilities']['experimentalApi'] is True
  elif method=='initialized': continue
  elif method=='account/read':
   refresh=r['params']['refreshToken'];calls.write('refreshToken='+str(refresh).lower()+'\n');calls.flush()
   if refresh:
    with open(p+'/auth.tmp','w') as f:json.dump({'tokens':{'access_token':'fresh-fixture','account_id':'fixture-account'}},f)
    os.replace(p+'/auth.tmp',p+'/auth.json')
   result={'account':{'type':'chatgpt'},'requiresOpenaiAuth':True}
  elif method=='account/rateLimits/read':
   assert r['params']=={};result={'rateLimits':{'primary':{'usedPercent':12.5,'windowDurationMins':300}}}
  else: sys.exit(9)
  print(json.dumps({'id':r['id'],'result':result}),flush=True)
"##,
    );
    manager.codex_command = executable.clone();
    let mut second = Manager::new(temp.path());
    second.codex_command = executable;
    use base64::Engine;
    let expired = format!(
        "e.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#)
    );
    config::write(
        Path::new(&a.profile).join("auth.json"),
        &serde_json::to_vec(&json!({"tokens":{"access_token":expired}})).unwrap(),
    )
    .unwrap();
    let manager = std::sync::Arc::new(manager);
    let second = std::sync::Arc::new(second);
    let mut tasks = vec![];
    for i in 0..20 {
        let m = if i % 2 == 0 {
            manager.clone()
        } else {
            second.clone()
        };
        let a = a.clone();
        tasks.push(tokio::spawn(async move { m.credentials(&a).await }));
    }
    for task in tasks {
        let c = task.await.unwrap().unwrap();
        assert_eq!(c.token, "fresh-fixture");
        assert_eq!(c.account_id, "fixture-account");
    }
    let s = manager.refresh(&a).await.unwrap();
    assert!(s.available);
    assert_eq!(s.windows.len(), 1);
    let calls = std::fs::read_to_string(Path::new(&a.profile).join("calls")).unwrap();
    assert_eq!(calls.matches("refreshToken=true").count(), 1);
    assert_eq!(calls.matches("refreshToken=false").count(), 1);
    assert!(config::lock(&a.profile, "native").is_ok());
}
#[cfg(unix)]
#[tokio::test]
async fn native_cancellation_kills_process_and_releases_profile() {
    let (temp, mut m, a) = fixture("codex");
    m.codex_command = script(
        temp.path(),
        "hung-codex",
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$CODEX_HOME/pid\"\nsleep 60\n",
    );
    let account = a.clone();
    let task = tokio::spawn(async move { m.refresh(&account).await });
    let pid_file = Path::new(&a.profile).join("pid");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake native process did not start");
    task.abort();
    let _ = task.await;
    assert!(config::lock(&a.profile, "native").is_ok());
    let pid: libc::pid_t = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    for _ in 0..20 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("native child survived cancellation");
}

#[cfg(unix)]
#[tokio::test]
async fn claude_control_passive_protocol_and_unexpected_activity() {
    let (temp, mut m, a) = fixture("claude");
    m.claude_command = script(
        temp.path(),
        "fake-claude",
        r##"#!/usr/bin/env python3
import json,os,sys
assert '--tools' in sys.argv and '--no-session-persistence' in sys.argv
assert os.environ['CLAUDE_CODE_ENTRYPOINT']=='sdk-go'
first=json.loads(sys.stdin.readline());assert first=={'request_id':'noria_initialize','type':'control_request','request':{'subtype':'initialize'}}
print(json.dumps({'type':'control_response','response':{'request_id':'noria_initialize','subtype':'success','response':{}}}),flush=True)
usage=json.loads(sys.stdin.readline());assert usage['request']=={'subtype':'get_usage','skip_behaviors':True}
print(json.dumps({'type':'control_response','response':{'request_id':'noria_usage','subtype':'success','response':{'rate_limits_available':True,'rate_limits':{'five_hour':{'utilization':20,'resets_at':None}}}}}),flush=True)
"##,
    );
    let s = m.refresh(&a).await.unwrap();
    assert_eq!(s.windows.len(), 1);
    assert_eq!(s.windows[0].used, Some(20.0));
    m.claude_command = script(
        temp.path(),
        "bad-claude",
        "#!/bin/sh\nread -r request\nprintf '%s\\n' '{\"type\":\"assistant\"}'\n",
    );
    assert!(
        m.refresh(&a)
            .await
            .unwrap_err()
            .to_string()
            .contains("unexpected Claude activity")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_oauth_rotation_is_single_persisted_and_preserves_unknown_fields() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (temp, mut m, a) = fixture("claude");
    let p = Path::new(&a.profile);
    let path = p.join(".credentials.json");
    config::write(&path,&serde_json::to_vec(&json!({"keep":"untouched","claudeAiOauth":{"accessToken":"expired-fixture","refreshToken":"refresh-fixture","expiresAt":1,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}})).unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    m.claude_token_url = format!("http://{addr}/token");
    let mut other = Manager::new(temp.path());
    other.claude_token_url = m.claude_token_url.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![];
        let mut buffer = [0; 4096];
        loop {
            let n = stream.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..split]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= split + 4 + length {
                    let request: serde_json::Value =
                        serde_json::from_slice(&bytes[split + 4..]).unwrap();
                    assert_eq!(request["refresh_token"], "refresh-fixture");
                    assert_eq!(request["scope"], "user:inference user:profile");
                    break;
                }
            }
        }
        let body = r#"{"access_token":"rotated-fixture","refresh_token":"rotated-refresh","expires_in":3600}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(400), listener.accept())
                .await
                .is_err()
        );
    });
    let m = std::sync::Arc::new(m);
    let other = std::sync::Arc::new(other);
    let mut tasks = vec![];
    for i in 0..10 {
        let m = if i % 2 == 0 { m.clone() } else { other.clone() };
        let a = a.clone();
        tasks.push(tokio::spawn(async move { m.credentials(&a).await }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap().token, "rotated-fixture");
    }
    server.await.unwrap();
    let root: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(root["keep"], "untouched");
    assert_eq!(root["claudeAiOauth"]["subscriptionType"], "max");
    assert_eq!(root["claudeAiOauth"]["refreshToken"], "rotated-refresh");
}

#[tokio::test]
async fn failed_oauth_refresh_is_redacted_and_leaves_native_file_unchanged() {
    for (status, body) in [
        (400, r#"{"error":"private-server-secret"}"#),
        (200, r#"{"expires_in":28800}"#),
        (200, r#"{"access_token":"bad","expires_in":0}"#),
        (200, "{not-json}"),
    ] {
        let (_temp, mut manager, account) = fixture("claude");
        let path = Path::new(&account.profile).join(".credentials.json");
        let original = br#"{"claudeAiOauth":{"accessToken":"expired","refreshToken":"private-refresh","expiresAt":1},"extra":{"keep":true}}"#;
        config::write(&path, original).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        manager.claude_token_url = format!("http://{}/token", listener.local_addr().unwrap());
        let router = axum::Router::new().fallback(move || async move {
            (axum::http::StatusCode::from_u16(status).unwrap(), body)
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let error = manager.credentials(&account).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(!text.contains("private-server-secret"));
        assert!(!text.contains("private-refresh"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        server.abort();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn claude_null_control_uses_same_profile_oauth_without_user_turn() {
    let (temp, mut manager, account) = fixture("claude");
    manager.claude_command = script(
        temp.path(),
        "null-claude",
        r##"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
 r=json.loads(line)
 assert r['type']=='control_request'
 subtype=r['request']['subtype']
 assert subtype in ('initialize','get_usage')
 result={} if subtype=='initialize' else {'rate_limits_available':True,'rate_limits':None}
 print(json.dumps({'type':'control_response','response':{'request_id':r['request_id'],'subtype':'success','response':result}}),flush=True)
"##,
    );
    config::write(
        Path::new(&account.profile).join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"same-profile-fixture","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    manager.claude_usage_url = format!("http://{}/usage", listener.local_addr().unwrap());
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let router = axum::Router::new().fallback(move |headers:axum::http::HeaderMap| {
        let counter = counter.clone();
        async move {
            assert_eq!(headers["authorization"], "Bearer same-profile-fixture");
            assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
            counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            axum::Json(json!({"five_hour":{"utilization":25,"resets_at":null},"seven_day_breakdown":{"rows":[]}}))
        }
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let snapshot = manager.refresh(&account).await.unwrap();
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].used, Some(25.0));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn native_browser_login_correlates_callback_and_reads_identity() {
    let (temp, mut manager, account) = fixture("codex");
    manager.login_opener = Some(|url| {
        assert_eq!(url, "https://auth.openai.com/fixture");
        Ok(())
    });
    manager.codex_command = script(
        temp.path(),
        "login-codex",
        r##"#!/usr/bin/env python3
import base64,json,os,sys
assert os.environ['CODEX_HOME'] == os.getcwd()
assert 'cli_auth_credentials_store="file"' in sys.argv
for line in sys.stdin:
 r=json.loads(line)
 if r['method']=='initialize':
  print(json.dumps({'id':r['id'],'result':{}}),flush=True)
 elif r['method']=='initialized': pass
 elif r['method']=='account/login/start':
  assert r['params']=={'type':'chatgpt'}
  print(json.dumps({'id':r['id'],'result':{'authUrl':'https://auth.openai.com/fixture','loginId':'matching-login'}}),flush=True)
  print(json.dumps({'method':'account/login/completed','params':{'loginId':'unrelated-login','success':False}}),flush=True)
  token='fixture.'+base64.urlsafe_b64encode(b'{"email":"native@example.com"}').decode().rstrip('=')+'.fixture'
  with open('auth.json','w') as f:json.dump({'tokens':{'access_token':'fake-access','id_token':token,'account_id':'native-account'}},f)
  print(json.dumps({'method':'account/login/completed','params':{'loginId':'matching-login','success':True}}),flush=True)
 else: sys.exit(9)
"##,
    );
    manager.browser_login(&account).await.unwrap();
    let identity = manager.identity(&account).await.unwrap();
    assert!(identity.authenticated);
    assert_eq!(identity.email, "native@example.com");
    assert_eq!(identity.account_id, "native-account");
    assert!(config::lock(&account.profile, "native").is_ok());
}
