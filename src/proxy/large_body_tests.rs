use super::*;
use axum::extract::ws::{Message as NativeMessage, WebSocketUpgrade};
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn image_history_above_64_mib_forwards_verbatim_on_all_http_routes() {
    for (provider, route, prefix, suffix) in [
        (
            "codex",
            "/codex/v1/responses",
            r#"{"model":"test","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,"#,
            r#""}]}]}"#,
        ),
        (
            "codex",
            "/codex/v1/responses/compact",
            r#"{"model":"test","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,"#,
            r#""}]}]}"#,
        ),
        (
            "claude",
            "/claude/v1/messages",
            r#"{"model":"test","messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":""#,
            r#""}}]}],"max_tokens":1}"#,
        ),
    ] {
        let body = Bytes::from(format!("{prefix}{}{suffix}", "A".repeat(65 << 20)));
        let expected = body.clone();
        let length = body.len();
        let upstream = Router::new().fallback(any(move |request: AxumRequest| async move {
            let mut stream = request.into_body().into_data_stream();
            let mut count = 0;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                let end = count + chunk.len();
                assert!(
                    end <= length && chunk.as_ref() == &expected[count..end],
                    "upstream bytes changed at offset {count}"
                );
                count = end;
            }
            assert_eq!(count, length);
            Json(json!({"id":"ok"}))
        }));
        let fixture = fixture(upstream, provider, 1).await;
        let response = timeout(
            Duration::from_secs(30),
            post(&format!("{}{route}", fixture.url), body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.json::<Value>().await.unwrap()["id"], "ok");
    }
}
#[tokio::test]
async fn websocket_image_history_above_64_mib_forwards_verbatim() {
    let body = Bytes::from(format!(
        "{{\"type\":\"response.create\",\"model\":\"test\",\"input\":[{{\"role\":\"user\",\"content\":[{{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,{}\"}}]}}]}}",
        "A".repeat(65 << 20)
    ));
    let expected = body.clone();
    let length = body.len();
    let upstream = Router::new().fallback(any(move |ws: WebSocketUpgrade| async move {
        ws.max_message_size(MAX_BODY)
            .max_frame_size(MAX_BODY)
            .on_upgrade(move |mut socket| async move {
                let NativeMessage::Text(message) = socket.recv().await.unwrap().unwrap() else {
                    panic!("expected native text")
                };
                assert_eq!(message.len(), length);
                assert!(
                    message.as_bytes() == expected.as_ref(),
                    "upstream WebSocket bytes changed"
                );
                socket
                    .send(NativeMessage::Text(
                        r#"{"type":"response.completed","response":{"id":"ok"}}"#.into(),
                    ))
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            })
    }));
    let fixture = fixture(upstream, "codex", 1).await;
    let mut socket = super::websocket_tests::connect(&fixture.url).await;
    timeout(
        Duration::from_secs(30),
        socket.send(Message::Text(body.try_into().unwrap())),
    )
    .await
    .unwrap()
    .unwrap();
    let message = timeout(Duration::from_secs(30), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(message.into_text().unwrap().contains("response.completed"));
}
