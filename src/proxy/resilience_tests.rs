use super::*;
use futures_util::SinkExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn only_valid_empty_lifecycle_schemas_are_harmless() {
    for value in [
        r#"{"type":"message_start","message":{"content":"fixture output"}}"#,
        r#"{"type":"message_start","message":{"content":{}}}"#,
        r#"{"type":"message_start","message":"fixture output"}"#,
        r#"{"type":"message_start","message":{"content":[null]}}"#,
        r#"{"type":"response.created","message":{"content":4}}"#,
        r#"{"type":4}"#,
    ] {
        assert!(
            classification::meaningful_event(value.as_bytes()),
            "malformed or nonempty lifecycle was held: {value}"
        );
    }
    for value in [
        r#"{"type":"message_start","message":{"content":[]}}"#,
        r#"{"type":"message_start","message":{"content":null}}"#,
        r#"{"type":"message_start","message":null}"#,
        r#"{"type":"message_start"}"#,
        r#"{"type":"response.created"}"#,
    ] {
        assert!(!classification::meaningful_event(value.as_bytes()));
    }
}

#[tokio::test]
async fn malformed_lifecycle_output_prevents_sse_and_websocket_replay() {
    const MALFORMED: &str = r#"{"type":"message_start","message":{"content":"fixture output"}}"#;
    const REJECTION: &str =
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached"}}"#;
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let upstream = Router::new().fallback(any(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            (
                [("content-type", "text/event-stream")],
                format!("data: {MALFORMED}\n\ndata: {REJECTION}\n\n"),
            )
        }
    }));
    let http_fixture = fixture(upstream, "claude", 2).await;
    let response = post(&format!("{}/claude/v1/messages", http_fixture.url), "{}").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        format!("data: {MALFORMED}\n\ndata: {REJECTION}\n\n")
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let upstream = Router::new().fallback(any(move |ws: axum::extract::ws::WebSocketUpgrade| {
        let count = count.clone();
        async move {
            ws.on_upgrade(move |mut socket| async move {
                count.fetch_add(1, Ordering::SeqCst);
                socket.recv().await.unwrap().unwrap();
                for text in [MALFORMED, REJECTION] {
                    socket
                        .send(axum::extract::ws::Message::Text(text.into()))
                        .await
                        .unwrap();
                }
                std::future::pending::<()>().await;
            })
        }
    }));
    let ws_fixture = fixture(upstream, "codex", 2).await;
    let mut socket = super::websocket_tests::connect(&ws_fixture.url).await;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"response.create","model":"test"}"#.into(),
        ))
        .await
        .unwrap();
    for expected in [MALFORMED, REJECTION] {
        let frame = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame.into_text().unwrap().as_str(), expected);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn session_hash_is_compatible_with_go_json_html_and_line_escaping() {
    let bytes = "{\"session_id\":\"a<b&>\u{2028}\"}";
    let object = parse_request(bytes.as_bytes()).unwrap();
    let expected = Sha256::digest(br#"["session_id","a\u003cb\u0026\u003e\u2028"]"#)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(headers::session_key(&HeaderMap::new(), &object), expected);
}

#[test]
fn ipv4_mapped_loopback_hosts_origins_and_endpoints_match_go() {
    for host in [
        "127.0.0.1:43119",
        "[::1]:43119",
        "[::ffff:127.0.0.1]:43119",
        "[::ffff:7f00:1]:43119",
    ] {
        assert!(headers::local_host(host), "loopback host rejected: {host}");
        let mut values = HeaderMap::new();
        values.insert("host", HeaderValue::from_str(host).unwrap());
        values.insert(
            "origin",
            HeaderValue::from_str(&format!("http://{host}")).unwrap(),
        );
        assert!(headers::valid_origin(&values));
        assert!(
            headers::endpoint(
                &Account {
                    endpoint: format!("http://{host}"),
                    ..Default::default()
                },
                "/responses",
                ""
            )
            .is_ok()
        );
    }
    for host in ["[::ffff:192.0.2.1]:43119", "[::1]evil", "[::1]:43119:extra"] {
        assert!(!headers::local_host(host));
    }
    let endpoint = headers::endpoint(
        &Account {
            endpoint: "http://127.0.0.1:9000/native//".into(),
            ..Default::default()
        },
        "/responses",
        "",
    )
    .unwrap();
    assert_eq!(endpoint.path(), "/native//responses");
}

#[tokio::test]
async fn cancellation_before_headers_closes_upstream_and_releases_reservation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut scratch = [0; 4096];
        loop {
            let count = socket.read(&mut scratch).await.unwrap();
            assert_ne!(count, 0);
            received.extend_from_slice(&scratch[..count]);
            if received.ends_with(b"\r\n\r\n{}") {
                break;
            }
        }
        started_tx.send(()).unwrap();
        assert_eq!(socket.read(&mut scratch).await.unwrap(), 0);
        let _ = closed_tx.send(());
    });
    let home = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(
        Scheduler::new(
            home.path().join("state.json"),
            vec![Account {
                id: "a".into(),
                provider: "codex".into(),
                enabled: true,
                endpoint,
                ..Default::default()
            }],
        )
        .unwrap(),
    );
    let gateway = Arc::new(Server::new(
        scheduler,
        Arc::new(FakeBackend),
        "local-secret".into(),
    ));
    let (url, server) = serve(gateway.router()).await;
    let url = format!("{url}/responses");
    let request = tokio::spawn(async move { post(&url, "{}").await });
    timeout(Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();
    request.abort();
    let _ = request.await;
    timeout(Duration::from_secs(2), closed_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gateway.active_calls(), 0);
    assert_eq!(gateway.scheduler.status().accounts[0].inflight, 0);
    gateway.shutdown().await;
    server.abort();
    upstream.await.unwrap();
}

#[tokio::test]
async fn affinity_persistence_failure_stops_before_any_stream_output() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let block = state.clone();
    let upstream = Router::new().fallback(any(move || {
        let block = block.clone();
        async move {
            std::fs::remove_dir_all(&block).unwrap();
            std::fs::write(&block, "prevent state persistence").unwrap();
            (
                [("content-type", "text/event-stream")],
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"must not commit\"}\n\n",
            )
        }
    }));
    let (endpoint, remote) = serve(upstream).await;
    let scheduler = Arc::new(
        Scheduler::new(
            state.join("scheduler.json"),
            vec![Account {
                id: "a".into(),
                provider: "codex".into(),
                enabled: true,
                endpoint,
                ..Default::default()
            }],
        )
        .unwrap(),
    );
    let gateway = Arc::new(Server::new(
        scheduler,
        Arc::new(FakeBackend),
        "local-secret".into(),
    ));
    let (url, server) = serve(gateway.router()).await;
    let response = post(&format!("{url}/responses"), "{}").await;
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert!(body.contains("cannot persist session affinity"));
    assert!(!body.contains("must not commit"));
    assert_eq!(gateway.scheduler.status().accounts[0].inflight, 0);
    gateway.shutdown().await;
    server.abort();
    remote.abort();
}

#[tokio::test]
async fn transport_failure_does_not_replay_an_uncertain_generation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let attempts = count.clone();
    let upstream = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            attempts.fetch_add(1, Ordering::SeqCst);
            let mut scratch = [0; 4096];
            let _ = socket.read(&mut scratch).await;
            let _ = socket.shutdown().await;
        }
    });
    let home = tempfile::tempdir().unwrap();
    let accounts = ["a", "b"]
        .iter()
        .map(|id| Account {
            id: (*id).into(),
            provider: "codex".into(),
            enabled: true,
            endpoint: endpoint.clone(),
            ..Default::default()
        })
        .collect();
    let scheduler = Arc::new(Scheduler::new(home.path().join("state.json"), accounts).unwrap());
    let gateway = Arc::new(Server::new(
        scheduler,
        Arc::new(FakeBackend),
        "local-secret".into(),
    ));
    let (url, server) = serve(gateway.router()).await;
    let response = post(&format!("{url}/responses"), "{}").await;
    assert_eq!(response.status(), 502);
    response.bytes().await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.scheduler.status().bindings, 0);
    gateway.shutdown().await;
    server.abort();
    upstream.abort();
}

#[tokio::test]
async fn unavailable_forecast_comes_only_from_scheduler_evidence() {
    for retry in [None, chrono::DateTime::from_timestamp(2_000_000_000, 0)] {
        let error = anyhow::Error::new(UnavailableError {
            provider: "codex".into(),
            model: String::new(),
            retry_at: retry,
        });
        let response = fail_unavailable(&error);
        assert_eq!(response.status(), 503);
        let header = response.headers().get("retry-after").cloned();
        let body = String::from_utf8(
            to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        if let Some(reset) = retry {
            assert_eq!(
                httpdate::parse_http_date(header.unwrap().to_str().unwrap()).unwrap(),
                reset.into()
            );
            assert!(body.contains(&reset.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
        } else {
            assert!(header.is_none());
            assert!(!body.contains("retry_at"));
        }
    }
}
