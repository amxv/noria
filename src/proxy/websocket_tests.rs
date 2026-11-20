use super::*;
use axum::extract::ws::{Message as NativeMessage, WebSocketUpgrade};
use futures_util::SinkExt;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

pub(super) async fn connect(url: &str) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("{}/v1/responses", url.replacen("http://", "ws://", 1))
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer local-secret"),
    );
    request
        .headers_mut()
        .insert("session_id", HeaderValue::from_static("ws-session"));
    connect_async(request).await.unwrap().0
}
async fn text(socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>) -> String {
    timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap()
        .to_string()
}
#[tokio::test]
async fn early_quota_failover_preserves_frames_and_reuses_connection() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let create =
        r#"{"type":"response.create","model":"test","input":[{"role":"user","content":"hi"}]}"#;
    let completed = r#"{"type":"response.completed","response":{"id":"r1"}}"#;
    let upstream=Router::new().fallback(any(move |ws:WebSocketUpgrade,headers:HeaderMap| { let count=count.clone(); async move {
        ws.on_upgrade(move |mut socket|async move {
            count.fetch_add(1,Ordering::SeqCst);
            while let Some(Ok(NativeMessage::Text(body)))=socket.recv().await {
                assert_eq!(body.as_str(),create);
                socket.send(NativeMessage::Text(r#"{"type":"response.created"}"#.into())).await.unwrap();
                if header(&headers,"authorization")=="Bearer token-a" { socket.send(NativeMessage::Text(r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached"}}"#.into())).await.unwrap(); return; }
                socket.send(NativeMessage::Text(completed.into())).await.unwrap();
            }
        })
    }}));
    let fixture = fixture(upstream, "codex", 2).await;
    let mut socket = connect(&fixture.url).await;
    for _ in 0..2 {
        socket.send(Message::Text(create.into())).await.unwrap();
        assert_eq!(text(&mut socket).await, r#"{"type":"response.created"}"#);
        assert_eq!(text(&mut socket).await, completed);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    drop(socket);
    fixture.gateway.shutdown().await;
}
#[tokio::test]
async fn partial_output_is_never_retried_or_spliced() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let frames = [
        r#"{"type":"response.created"}"#,
        r#"{"type":"response.output_text.delta","delta":"first"}"#,
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached"}}"#,
    ];
    let upstream = Router::new().fallback(any(move |ws: WebSocketUpgrade| {
        let count = count.clone();
        async move {
            ws.on_upgrade(move |mut socket| async move {
                count.fetch_add(1, Ordering::SeqCst);
                socket.recv().await.unwrap().unwrap();
                for frame in frames {
                    socket
                        .send(NativeMessage::Text(frame.into()))
                        .await
                        .unwrap();
                }
                std::future::pending::<()>().await;
            })
        }
    }));
    let fixture = fixture(upstream, "codex", 2).await;
    let mut socket = connect(&fixture.url).await;
    socket
        .send(Message::Text(
            r#"{"type":"response.create","model":"test"}"#.into(),
        ))
        .await
        .unwrap();
    for expected in frames {
        assert_eq!(text(&mut socket).await, expected);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.gateway.scheduler.status().bindings, 1);
}
#[tokio::test]
async fn control_messages_disable_retry_before_output() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let upstream = Router::new().fallback(any(move |ws: WebSocketUpgrade| {
        let count = count.clone();
        async move {
            ws.on_upgrade(move |mut socket| async move {
                count.fetch_add(1, Ordering::SeqCst);
                socket.recv().await.unwrap().unwrap();
                let control = socket.recv().await.unwrap().unwrap();
                assert!(control.into_text().unwrap().contains("response.cancel"));
                socket
                    .send(NativeMessage::Text(
                        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached"}}"#
                            .into(),
                    ))
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            })
        }
    }));
    let fixture = fixture(upstream, "codex", 2).await;
    let mut socket = connect(&fixture.url).await;
    socket
        .send(Message::Text(
            r#"{"type":"response.create","model":"test"}"#.into(),
        ))
        .await
        .unwrap();
    socket
        .send(Message::Text(r#"{"type":"response.cancel"}"#.into()))
        .await
        .unwrap();
    assert!(text(&mut socket).await.contains("usage_limit_reached"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn prewarm_keeps_reservation_without_usage_until_real_generation() {
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let upstream = Router::new().fallback(any(move |ws: WebSocketUpgrade| {
        let count = count.clone();
        async move {
            ws.on_upgrade(move |mut socket| async move {
                count.fetch_add(1, Ordering::SeqCst);
                while let Some(Ok(_)) = socket.recv().await {
                    socket
                        .send(NativeMessage::Text(r#"{"type":"response.created"}"#.into()))
                        .await
                        .unwrap();
                    socket
                        .send(NativeMessage::Text(
                            r#"{"type":"response.completed"}"#.into(),
                        ))
                        .await
                        .unwrap();
                }
            })
        }
    }));
    let fixture = fixture(upstream, "codex", 2).await;
    for id in ["a", "b"] {
        fixture
            .gateway
            .scheduler
            .update(Snapshot {
                account_id: id.into(),
                observed_at: chrono::Utc::now(),
                available: true,
                windows: vec![crate::model::Window {
                    name: "five_hour".into(),
                    used: Some(0.0),
                    duration: 5 * 3600 * 1_000_000_000,
                    dormant: true,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
    }
    let mut socket = connect(&fixture.url).await;
    for (index,create) in [r#"{"type":"response.create","model":"test","generate":false}"#,r#"{"type":"response.create","model":"test","input":[{"role":"user","content":"hi"}],"previous_response_id":"warmup-id"}"#].iter().enumerate() {
        socket.send(Message::Text((*create).into())).await.unwrap(); text(&mut socket).await; text(&mut socket).await;
        let state=fixture.gateway.scheduler.status(); assert_eq!(state.bindings,index);
        assert_eq!(serde_json::to_string(&state).unwrap().contains("predicted_resets"),index==1);
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn delayed_handshake_quota_body_is_inspected_before_failover() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let upstream=Router::new().fallback(any(move |ws:WebSocketUpgrade,headers:HeaderMap| { let count=count.clone(); async move {
        count.fetch_add(1,Ordering::SeqCst);
        if header(&headers,"authorization")=="Bearer token-a" {
            let stream=async_stream::stream! { tokio::time::sleep(Duration::from_millis(30)).await; yield Ok::<_,std::io::Error>(Bytes::from_static(br#"{"error":{"code":"usage_limit_reached"}}"#)); };
            (StatusCode::TOO_MANY_REQUESTS,Body::from_stream(stream)).into_response()
        } else {
            ws.on_upgrade(|mut socket|async move { socket.recv().await.unwrap().unwrap(); socket.send(NativeMessage::Text(r#"{"type":"response.completed"}"#.into())).await.unwrap(); std::future::pending::<()>().await; }).into_response()
        }
    }}));
    let fixture = fixture(upstream, "codex", 2).await;
    let mut socket = connect(&fixture.url).await;
    socket
        .send(Message::Text(
            r#"{"type":"response.create","model":"test"}"#.into(),
        ))
        .await
        .unwrap();
    assert!(text(&mut socket).await.contains("response.completed"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn shutdown_closes_idle_upgraded_connections() {
    let fixture = fixture(Router::new().fallback(unexpected_upstream), "codex", 1).await;
    let mut socket = connect(&fixture.url).await;
    timeout(Duration::from_secs(1), fixture.gateway.shutdown())
        .await
        .unwrap();
    let frame = timeout(Duration::from_secs(1), socket.next())
        .await
        .unwrap();
    assert!(!matches!(frame, Some(Ok(Message::Text(_)))));
}
