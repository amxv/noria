use super::*;
use axum::{extract::Request as AxumRequest, routing::any};
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tower::ServiceExt;

#[path = "large_body_tests.rs"]
mod large_body_tests;
#[path = "resilience_tests.rs"]
mod resilience_tests;
#[path = "websocket_tests.rs"]
mod websocket_tests;

struct FakeBackend;
async fn unexpected_upstream() -> StatusCode {
    panic!("invalid or idle request escaped to upstream")
}
impl Backend for FakeBackend {
    fn credentials<'a>(
        &'a self,
        account: &'a Account,
    ) -> BoxFuture<'a, anyhow::Result<Credential>> {
        Box::pin(async move {
            Ok(Credential {
                token: format!("token-{}", account.id),
                account_id: format!("org-{}", account.id),
            })
        })
    }
    fn refresh<'a>(&'a self, _: &'a Account) -> BoxFuture<'a, anyhow::Result<Snapshot>> {
        Box::pin(async { anyhow::bail!("telemetry unavailable") })
    }
}
struct Fixture {
    gateway: Arc<Server>,
    url: String,
    local: JoinHandle<()>,
    remote: JoinHandle<()>,
    _home: TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.gateway.cancel.cancel();
        self.local.abort();
        self.remote.abort();
    }
}
async fn serve(router: Router) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (address, task)
}
async fn fixture(router: Router, provider: &str, count: usize) -> Fixture {
    fixture_backend(router, provider, count, Arc::new(FakeBackend)).await
}
async fn fixture_backend(
    router: Router,
    provider: &str,
    count: usize,
    backend: Arc<dyn Backend>,
) -> Fixture {
    let (remote_url, remote) = serve(router).await;
    let home = tempfile::tempdir().unwrap();
    let accounts = (0..count)
        .map(|index| Account {
            id: char::from(b'a' + index as u8).to_string(),
            provider: provider.into(),
            enabled: true,
            endpoint: remote_url.clone(),
            ..Account::default()
        })
        .collect();
    let scheduler = Arc::new(Scheduler::new(home.path().join("state.json"), accounts).unwrap());
    let gateway = Arc::new(Server::new(scheduler, backend, "local-secret".into()));
    let (url, local) = serve(gateway.router()).await;
    Fixture {
        gateway,
        url,
        local,
        remote,
        _home: home,
    }
}
async fn post(url: &str, body: impl Into<reqwest::Body>) -> reqwest::Response {
    reqwest::Client::new()
        .post(url)
        .header("authorization", "Bearer local-secret")
        .header("content-type", "application/json")
        .header("session_id", "conversation")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[test]
fn classifier_distinguishes_subscription_throughput_scope_and_permission() {
    use classification::classify;
    for (body, status, kind, scope, retry) in [
        (
            r#"{"error":{"type":"usage_limit_reached"}}"#,
            429,
            "quota",
            "",
            true,
        ),
        (
            r#"{"error":{"type":"model_limit_reached"}}"#,
            429,
            "quota",
            "test-model",
            true,
        ),
        (
            r#"{"error":{"type":"permission_error"}}"#,
            403,
            "",
            "",
            false,
        ),
        (
            r#"{"error":{"type":"authentication_error"}}"#,
            403,
            "auth",
            "",
            true,
        ),
        (
            r#"{"error":{"type":"rate_limit_error","message":"Too many requests per minute"}}"#,
            429,
            "throughput",
            "",
            false,
        ),
        (
            r#"{"error":{"type":"rate_limit_error","message":"You've hit your usage limit. Try again later."}}"#,
            429,
            "quota",
            "",
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"error":{"type":"rate_limit_error","message":"You've reached your limit; reset at 8:52 AM."}}}"#,
            0,
            "quota",
            "",
            true,
        ),
    ] {
        let (rejection, actual_retry) =
            classify(status, &HeaderMap::new(), body.as_bytes(), "test-model");
        assert_eq!(
            (
                rejection.kind.as_str(),
                rejection.model.as_str(),
                actual_retry
            ),
            (kind, scope, retry),
            "{body}"
        );
    }
    for text in [
        "Number of requests has exceeded your 7-day rate limit.",
        "Your weekly limit has been reached.",
    ] {
        let (rejection, retry) = classify(
            429,
            &HeaderMap::new(),
            json!({"error":{"type":"rate_limit_error","message":text}})
                .to_string()
                .as_bytes(),
            "claude-test",
        );
        assert!(retry);
        assert_eq!(rejection.window, "seven_day");
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-status",
        HeaderValue::from_static("rejected"),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        HeaderValue::from_static("2000000000"),
    );
    headers.insert(
        "anthropic-ratelimit-unified-7d-status",
        HeaderValue::from_static("rejected"),
    );
    headers.insert(
        "anthropic-ratelimit-unified-7d-reset",
        HeaderValue::from_static("2100000000"),
    );
    let (rejection, retry) = classify(
        429,
        &headers,
        br#"{"error":{"type":"rate_limit_error"}}"#,
        "model",
    );
    assert!(retry);
    assert_eq!(rejection.kind, "quota");
    assert_eq!(rejection.until.unwrap().timestamp(), 2100000000);
    assert!(rejection.window.is_empty());
    // Generated words resembling provider errors are never sufficient.
    assert!(
        !classify(
            0,
            &HeaderMap::new(),
            br#"{"type":"response.output_text.delta","delta":"usage_limit_reached"}"#,
            "model"
        )
        .1
    );
}
#[test]
fn endpoint_transport_and_native_headers_are_preserved() {
    for endpoint in [
        "http://example.com",
        "http://example.com:8080",
        "https://user:pass@example.com",
        "https://example.com?x=1",
    ] {
        assert!(
            headers::endpoint(
                &Account {
                    endpoint: endpoint.into(),
                    ..Account::default()
                },
                "/responses",
                ""
            )
            .is_err()
        );
    }
    for endpoint in [
        "http://127.0.0.1:9000",
        "http://[::1]:9000",
        "https://example.com",
    ] {
        assert!(
            headers::endpoint(
                &Account {
                    endpoint: endpoint.into(),
                    ..Account::default()
                },
                "/responses",
                ""
            )
            .is_ok()
        );
    }
    let mut original = HeaderMap::new();
    original.insert(
        "authorization",
        HeaderValue::from_static("Bearer local-secret"),
    );
    original.append(
        "anthropic-beta",
        HeaderValue::from_static("native-tools-2026-01-01"),
    );
    original.append(
        "anthropic-beta",
        HeaderValue::from_static("another-native-beta"),
    );
    original.insert("connection", HeaderValue::from_static("X-Hop-Only"));
    original.insert("x-hop-only", HeaderValue::from_static("secret"));
    let credential = Credential {
        token: "upstream-token".into(),
        account_id: String::new(),
    };
    let modified = headers::upstream_headers(&original, "claude", &credential).unwrap();
    assert_eq!(header(&original, "authorization"), "Bearer local-secret");
    assert_eq!(header(&modified, "authorization"), "Bearer upstream-token");
    for beta in [
        "native-tools-2026-01-01",
        "another-native-beta",
        "oauth-2025-04-20",
    ] {
        assert!(header(&modified, "anthropic-beta").contains(beta));
    }
    assert!(!modified.contains_key("x-hop-only"));
    let again = headers::upstream_headers(&modified, "claude", &credential).unwrap();
    assert_eq!(
        header(&again, "anthropic-beta")
            .matches("oauth-2025-04-20")
            .count(),
        1
    );
}
#[test]
fn session_precedence_and_branches_match_native_clients() {
    let mut headers = HeaderMap::new();
    let empty = RequestMeta::default();
    assert!(headers::session_key(&headers, &empty).is_empty());
    headers.insert("x-noria-session", HeaderValue::from_static("wrapper"));
    let wrapper = headers::session_key(&headers, &empty);
    headers.insert("session_id", HeaderValue::from_static("native"));
    let native = headers::session_key(&headers, &empty);
    assert_ne!(native, wrapper);
    headers.insert("x-claude-code-agent-id", HeaderValue::from_static("branch"));
    assert_ne!(headers::session_key(&headers, &empty), native);
    headers.remove("session_id");
    headers.remove("x-claude-code-agent-id");
    let object = parse_request(br#"{"prompt_cache_key":"branch-key"}"#).unwrap();
    assert_ne!(headers::session_key(&headers, &object), wrapper);
    let metadata =
        parse_request(br#"{"metadata":{"user_id":"{\"session_id\":\"native-claude\"}"}}"#).unwrap();
    assert_ne!(headers::session_key(&headers, &metadata), wrapper);
}
#[tokio::test]
async fn host_origin_auth_malformed_routes_and_limits_are_rejected() {
    let fixture = fixture(Router::new().fallback(unexpected_upstream), "codex", 1).await;
    for (host, origin, secret, path, body, status) in [
        ("evil.test", "", "local-secret", "/responses", "{}", 403),
        (
            "127.0.0.1",
            "https://evil.test",
            "local-secret",
            "/responses",
            "{}",
            403,
        ),
        (
            "127.0.0.1:43119",
            "http://127.0.0.1:80",
            "local-secret",
            "/responses",
            "{}",
            403,
        ),
        ("127.0.0.1", "", "wrong", "/responses", "{}", 401),
        ("127.0.0.1", "", "local-secret", "/responses", "null", 400),
        ("127.0.0.1", "", "local-secret", "/responses", "[]", 400),
        ("127.0.0.1", "", "local-secret", "/unknown", "{}", 404),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", host)
            .header("origin", origin)
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::from(body))
            .unwrap();
        assert_eq!(
            fixture
                .gateway
                .router()
                .oneshot(request)
                .await
                .unwrap()
                .status()
                .as_u16(),
            status
        );
    }
    let request = Request::builder()
        .method("POST")
        .uri("/responses")
        .header("host", "localhost")
        .header("authorization", "Bearer local-secret")
        .header("content-length", MAX_BODY + 1)
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        fixture
            .gateway
            .router()
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let request = Request::builder()
        .uri("/_noria/status")
        .header("host", "localhost")
        .header("authorization", "Bearer local-secret")
        .body(Body::empty())
        .unwrap();
    let response = fixture.gateway.router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for secret in ["local-secret", "token-", "profile"] {
        assert!(!body.contains(secret));
    }
}
#[tokio::test]
async fn native_body_headers_quota_failover_and_affinity_are_preserved() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let body = r#"{"model":"gpt-test","input":[{"type":"function_call_output","call_id":"c1","output":"verbatim"}],"reasoning":{"effort":"high"}}"#;
    let upstream = Router::new().fallback(any(move |request: AxumRequest| {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.uri().path(), "/responses");
            assert_eq!(request.uri().query(), Some("trace=1"));
            let a = header(request.headers(), "authorization") == "Bearer token-a";
            assert_eq!(
                header(request.headers(), "chatgpt-account-id"),
                if a { "org-a" } else { "org-b" }
            );
            for key in ["x-api-key", "x-noria-session", "cookie"] {
                assert!(!request.headers().contains_key(key));
            }
            assert_eq!(to_bytes(request.into_body(), MAX_BODY).await.unwrap(), body);
            if a {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error":{"type":"usage_limit_reached","resets_in_seconds":3600}})),
                )
                    .into_response()
            } else {
                Json(json!({"id":"ok","reasoning":"unchanged"})).into_response()
            }
        }
    }));
    let fixture = fixture(upstream, "codex", 2).await;
    for _ in 0..2 {
        let response = post(&format!("{}/v1/responses?trace=1", fixture.url), body).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.json::<Value>().await.unwrap()["id"], "ok");
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.gateway.scheduler.status().bindings, 1);
}
#[tokio::test]
async fn resumed_codex_session_fails_over_only_after_subscription_rejection() {
    let limited = Arc::new(AtomicBool::new(false));
    let counts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let flag = limited.clone();
    let counter = counts.clone();
    let upstream = Router::new().fallback(any(move |request: AxumRequest| { let flag=flag.clone(); let counts=counter.clone(); async move {
        let a = header(request.headers(),"authorization")=="Bearer token-a"; counts[usize::from(!a)].fetch_add(1,Ordering::SeqCst);
        let bytes = to_bytes(request.into_body(),MAX_BODY).await.unwrap(); assert!(String::from_utf8_lossy(&bytes).contains("retained-history"));
        if a && flag.load(Ordering::SeqCst) { (StatusCode::TOO_MANY_REQUESTS,Json(json!({"error":{"type":"rate_limit_error","message":"You've hit your usage limit; resets at 8:52 AM."}}))).into_response() }
        else { Json(json!({"result":"ok"})).into_response() }
    }}));
    let fixture = fixture(upstream, "codex", 2).await;
    for _ in 0..3 {
        let response = post(
            &format!("{}/responses", fixture.url),
            r#"{"model":"gpt-test","continuation":"retained-history"}"#,
        )
        .await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        limited.store(true, Ordering::SeqCst);
    }
    assert_eq!(counts[0].load(Ordering::SeqCst), 2);
    assert_eq!(counts[1].load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn sse_retries_only_before_meaningful_output() {
    for partial in [false, true] {
        let count = Arc::new(AtomicUsize::new(0));
        let attempts = count.clone();
        let preamble = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";
        let output = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"usage_limit_reached\"}}}\n\n";
        let upstream = Router::new().fallback(any(move |headers: HeaderMap| {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                let a = header(&headers, "authorization") == "Bearer token-a";
                let body = if a {
                    format!("{preamble}{}{failed}", if partial { output } else { "" })
                } else {
                    format!("{preamble}{output}data: {{\"type\":\"response.completed\"}}\n\n")
                };
                ([("content-type", "text/event-stream")], body)
            }
        }));
        let fixture = fixture(upstream, "codex", 2).await;
        let result = post(
            &format!("{}/responses", fixture.url),
            r#"{"model":"test","stream":true}"#,
        )
        .await
        .text()
        .await
        .unwrap();
        if partial {
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert_eq!(result, format!("{preamble}{output}{failed}"));
        } else {
            assert_eq!(count.load(Ordering::SeqCst), 2);
            assert!(!result.contains("usage_limit_reached"));
            assert!(result.starts_with(&format!("{preamble}{output}")));
        }
    }
}
#[tokio::test]
async fn claude_lifecycle_quota_retries_and_throughput_does_not() {
    for quota in [false, true] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"content\":[]}}\n\n";
        let stop = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let upstream = Router::new().fallback(any(move |request: AxumRequest| { let count=count.clone(); async move {
            count.fetch_add(1,Ordering::SeqCst); assert_eq!(request.uri().path(),"/v1/messages");
            assert!(header(request.headers(),"anthropic-beta").contains("oauth-2025-04-20"));
            let a=header(request.headers(),"authorization")=="Bearer token-a";
            if quota {
                let end = if a { "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"You have reached your 5-hour usage limit\"}}\n\n" } else { stop };
                ([("content-type","text/event-stream")],format!("{start}{end}")).into_response()
            } else { (StatusCode::TOO_MANY_REQUESTS,Json(json!({"error":{"type":"rate_limit_error","message":"Too many requests per minute"}}))).into_response() }
        }}));
        let fixture = fixture(upstream, "claude", 2).await;
        let response = post(
            &format!("{}/claude/v1/messages", fixture.url),
            r#"{"model":"claude-test"}"#,
        )
        .await;
        if quota {
            assert_eq!(response.text().await.unwrap(), format!("{start}{stop}"));
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
        } else {
            assert_eq!(response.status(), 429);
            response.bytes().await.unwrap();
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }
}
#[tokio::test]
async fn drain_admission_is_atomic_and_busy_does_not_disable_service() {
    let fixture = fixture(Router::new().fallback(|| async { "{}" }), "codex", 1).await;
    let first = fixture.gateway.begin_call().unwrap();
    assert!(fixture.gateway.drain());
    let second = fixture.gateway.begin_call().unwrap();
    drop(first);
    drop(second);
    assert!(!fixture.gateway.drain());
    assert!(fixture.gateway.begin_call().is_none());
    assert_eq!(
        post(&format!("{}/responses", fixture.url), "{}")
            .await
            .status(),
        503
    );
    fixture.gateway.resume();
    assert!(fixture.gateway.begin_call().is_some());
}
#[tokio::test]
async fn stream_flushes_and_disconnect_releases_upstream_and_lease() {
    struct Closed(Arc<AtomicBool>);
    impl Drop for Closed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let closed = Arc::new(AtomicBool::new(false));
    let upstream_closed = closed.clone();
    let upstream=Router::new().fallback(any(move || { let closed=upstream_closed.clone(); async move {
        let stream=async_stream::stream! {
            let _closed=Closed(closed);
            yield Ok::<_,std::io::Error>(Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n"));
            std::future::pending::<()>().await;
        };
        ([("content-type","text/event-stream")],Body::from_stream(stream))
    }}));
    let fixture = fixture(upstream, "codex", 1).await;
    let response = timeout(
        Duration::from_secs(2),
        post(&format!("{}/responses", fixture.url), "{}"),
    )
    .await
    .unwrap();
    let mut stream = response.bytes_stream();
    let first = timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("first"));
    assert_eq!(fixture.gateway.active_calls(), 1);
    drop(stream);
    timeout(Duration::from_secs(2), async {
        while !closed.load(Ordering::SeqCst) || fixture.gateway.active_calls() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        fixture
            .gateway
            .scheduler
            .status()
            .accounts
            .iter()
            .all(|v| v.inflight == 0)
    );
}
#[tokio::test]
async fn quota_refresh_is_independent_and_shutdown_cancels_it() {
    struct Stalled {
        started: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    }
    struct Stopped(Arc<AtomicBool>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl Backend for Stalled {
        fn credentials<'a>(&'a self, a: &'a Account) -> BoxFuture<'a, anyhow::Result<Credential>> {
            Box::pin(async move {
                Ok(Credential {
                    token: format!("token-{}", a.id),
                    account_id: String::new(),
                })
            })
        }
        fn refresh<'a>(&'a self, _: &'a Account) -> BoxFuture<'a, anyhow::Result<Snapshot>> {
            Box::pin(async move {
                let _stopped = Stopped(self.stopped.clone());
                self.started.store(true, Ordering::SeqCst);
                std::future::pending().await
            })
        }
    }
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let upstream = Router::new().fallback(any(|headers: HeaderMap| async move {
        if header(&headers, "authorization") == "Bearer token-a" {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":{"code":"usage_limit_reached"}})),
            )
                .into_response()
        } else {
            Json(json!({"id":"b"})).into_response()
        }
    }));
    let fixture = fixture_backend(
        upstream,
        "codex",
        2,
        Arc::new(Stalled {
            started: started.clone(),
            stopped: stopped.clone(),
        }),
    )
    .await;
    let response = timeout(
        Duration::from_secs(1),
        post(&format!("{}/responses", fixture.url), "{}"),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert!(started.load(Ordering::SeqCst));
    timeout(Duration::from_secs(1), fixture.gateway.shutdown())
        .await
        .unwrap();
    assert!(stopped.load(Ordering::SeqCst));
}
#[tokio::test]
async fn expired_claude_credentials_are_actionable_and_redacted() {
    struct Expired;
    impl Backend for Expired {
        fn credentials<'a>(&'a self, _: &'a Account) -> BoxFuture<'a, anyhow::Result<Credential>> {
            Box::pin(async {
                anyhow::bail!("refresh rejected; sign in again; secret-not-for-client")
            })
        }
        fn refresh<'a>(&'a self, _: &'a Account) -> BoxFuture<'a, anyhow::Result<Snapshot>> {
            Box::pin(async { anyhow::bail!("unavailable") })
        }
    }
    let fixture = fixture_backend(
        Router::new().fallback(unexpected_upstream),
        "claude",
        1,
        Arc::new(Expired),
    )
    .await;
    let response = post(&format!("{}/claude/v1/messages", fixture.url), "{}").await;
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert!(body.contains("sign in again"));
    assert!(!body.contains("secret-not-for-client"));
    assert!(!body.contains("refresh quota"));
}
