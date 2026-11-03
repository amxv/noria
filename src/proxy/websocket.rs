use super::{
    classification::{classify, meaningful_event},
    headers::{endpoint, raw_string, session_key, upstream_headers},
    *,
};
use axum::extract::ws::{Message as ClientMessage, WebSocket};
use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use tokio::time::{Instant as TokioInstant, timeout, timeout_at};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};

type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;
struct Pending {
    body: Bytes,
    model: String,
    session: String,
    lease: Option<LeaseGuard>,
    call: Option<CallGuard>,
    excluded: HashSet<String>,
    attempts: usize,
    committed: bool,
    no_retry: bool,
    warmup: bool,
    held: Vec<ClientMessage>,
    held_bytes: usize,
}
enum Event {
    Client(Option<Result<ClientMessage, axum::Error>>),
    Upstream(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Timeout,
}
enum HandshakeFailure {
    Rejected(Rejection),
    Other,
}

impl Server {
    pub(super) async fn serve_websocket(
        self: Arc<Self>,
        mut client: WebSocket,
        mut headers: HeaderMap,
        query: String,
    ) {
        if self.cancel.is_cancelled() {
            return;
        }
        if header(&headers, "x-noria-session").is_empty() {
            headers.insert(
                "x-noria-session",
                HeaderValue::from_str(&uuid::Uuid::new_v4().simple().to_string()).expect("hex ID"),
            );
        }
        let mut upstream: Option<UpstreamSocket> = None;
        let mut account_id = String::new();
        let mut pending: Option<Pending> = None;
        let mut warmed: Option<Pending> = None;
        let mut client_deadline = TokioInstant::now() + CALL_TIMEOUT;
        let mut upstream_deadline = client_deadline;
        loop {
            let event = tokio::select! {
                _ = self.cancel.cancelled() => return,
                event = timeout_at(client_deadline,client.recv()) => event.map(Event::Client).unwrap_or(Event::Timeout),
                event = async {
                    match upstream.as_mut() {
                        Some(socket) => timeout_at(upstream_deadline,socket.next()).await.map(Event::Upstream).unwrap_or(Event::Timeout),
                        None => std::future::pending::<Event>().await,
                    }
                } => event,
            };
            match event {
                Event::Timeout => return,
                Event::Client(Some(Ok(message))) => {
                    client_deadline = TokioInstant::now() + CALL_TIMEOUT;
                    let text = match message {
                        ClientMessage::Text(text) => text,
                        ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                        ClientMessage::Close(_) => return,
                        _ => {
                            write_error(
                                &mut client,
                                &self.cancel,
                                "native JSON text messages required",
                            )
                            .await;
                            return;
                        }
                    };
                    let object = match parse_request(text.as_bytes()) {
                        Ok(object) => object,
                        Err(_) => {
                            write_error(&mut client, &self.cancel, "JSON object required").await;
                            return;
                        }
                    };
                    let kind = raw_string(object.kind);
                    if kind == "response.create" {
                        if pending.is_some() {
                            write_error(&mut client,&self.cancel,"one active response per connection is supported; use another connection for concurrent responses").await;
                            return;
                        }
                        let Some(call) = self.begin_call() else {
                            write_error(
                                &mut client,
                                &self.cancel,
                                "router is updating; reconnect shortly",
                            )
                            .await;
                            return;
                        };
                        let model = raw_string(object.model);
                        let session = session_key(&headers, &object);
                        let warmup = object
                            .generate
                            .and_then(|v| serde_json::from_str::<bool>(v.get()).ok())
                            == Some(false);
                        pending = Some(Pending {
                            body: text.into(),
                            model,
                            session,
                            warmup,
                            call: Some(call),
                            lease: None,
                            excluded: HashSet::new(),
                            attempts: 0,
                            committed: false,
                            no_retry: false,
                            held: Vec::new(),
                            held_bytes: 0,
                        });
                        if let Err(details) = self
                            .start_response(
                                &headers,
                                &query,
                                pending.as_mut().unwrap(),
                                &mut warmed,
                                &mut upstream,
                                &mut account_id,
                            )
                            .await
                        {
                            write_details(&mut client, &self.cancel, details).await;
                            return;
                        }
                        upstream_deadline = TokioInstant::now() + CALL_TIMEOUT;
                    } else {
                        if let Some(pending) = &mut pending {
                            pending.no_retry = true;
                        }
                        let Some(upstream) = &mut upstream else {
                            write_error(
                                &mut client,
                                &self.cancel,
                                "response.create is required before control messages",
                            )
                            .await;
                            return;
                        };
                        let bytes: Bytes = text.into();
                        if !write_upstream(
                            upstream,
                            Message::Text(bytes.try_into().expect("validated UTF-8")),
                            &self.cancel,
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
                Event::Client(_) => return,
                Event::Upstream(Some(Ok(message))) => {
                    upstream_deadline = TokioInstant::now() + CALL_TIMEOUT;
                    let frame = match message {
                        Message::Text(text) => {
                            let bytes: Bytes = text.into();
                            ClientMessage::Text(bytes.try_into().expect("validated UTF-8"))
                        }
                        Message::Binary(bytes) => ClientMessage::Binary(bytes),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        Message::Close(_) => {
                            if pending.is_some() {
                                write_error(
                                    &mut client,
                                    &self.cancel,
                                    "upstream connection closed; generation was not retried",
                                )
                                .await;
                            }
                            return;
                        }
                        Message::Frame(_) => continue,
                    };
                    let Some(active) = pending.as_mut() else {
                        if !write_client(&mut client, frame, &self.cancel).await {
                            return;
                        }
                        continue;
                    };
                    let bytes = match &frame {
                        ClientMessage::Text(text) => text.as_bytes(),
                        ClientMessage::Binary(bytes) => bytes,
                        _ => unreachable!(),
                    };
                    let (rejection, retry) = classify(0, &HeaderMap::new(), bytes, &active.model);
                    if retry
                        && !active.committed
                        && !active.no_retry
                        && active.attempts < MAX_ATTEMPTS
                    {
                        let lease = active.lease.as_ref().expect("started response lease");
                        if self.reject(&lease.lease, rejection).is_err() {
                            write_error(
                                &mut client,
                                &self.cancel,
                                "cannot persist account rejection",
                            )
                            .await;
                            return;
                        }
                        active.excluded.insert(lease.lease.account.id.clone());
                        active.lease = None;
                        upstream = None;
                        active.held.clear();
                        active.held_bytes = 0;
                        if let Err(details) = self
                            .start_response(
                                &headers,
                                &query,
                                active,
                                &mut warmed,
                                &mut upstream,
                                &mut account_id,
                            )
                            .await
                        {
                            write_details(&mut client, &self.cancel, details).await;
                            return;
                        }
                        upstream_deadline = TokioInstant::now() + CALL_TIMEOUT;
                        continue;
                    }
                    let terminal = serde_json::from_slice::<Value>(bytes)
                        .ok()
                        .and_then(|v| v.get("type").and_then(Value::as_str).map(String::from))
                        .unwrap_or_default();
                    let meaningful = meaningful_event(bytes);
                    if !active.committed {
                        active.held_bytes += bytes.len();
                        active.held.push(frame);
                        if !meaningful && active.held_bytes < MAX_PRELUDE {
                            continue;
                        }
                        let lease = &active.lease.as_ref().expect("started response lease").lease;
                        if retry {
                            let _ = self.scheduler.reject(lease, rejection);
                        } else if !active.warmup && self.scheduler.accept(lease).is_err() {
                            write_error(
                                &mut client,
                                &self.cancel,
                                "cannot persist session affinity",
                            )
                            .await;
                            return;
                        }
                        active.committed = true;
                        for held in active.held.drain(..) {
                            if !write_client(&mut client, held, &self.cancel).await {
                                return;
                            }
                        }
                    } else if !write_client(&mut client, frame, &self.cancel).await {
                        return;
                    }
                    if matches!(
                        terminal.as_str(),
                        "response.completed"
                            | "response.failed"
                            | "response.incomplete"
                            | "response.cancelled"
                            | "error"
                    ) {
                        let mut finished = pending.take().expect("active response");
                        if finished.warmup && terminal == "response.completed" {
                            finished.call = None;
                            warmed = Some(finished);
                        } else {
                            // Persist completion before drain can observe an
                            // idle router and begin the update handoff.
                            finished.lease = None;
                            finished.call = None;
                        }
                    }
                }
                Event::Upstream(_) => {
                    if pending.is_some() {
                        write_error(
                            &mut client,
                            &self.cancel,
                            "upstream connection closed; generation was not retried",
                        )
                        .await;
                    }
                    return;
                }
            }
        }
    }

    async fn start_response(
        self: &Arc<Self>,
        headers: &HeaderMap,
        query: &str,
        pending: &mut Pending,
        warmed: &mut Option<Pending>,
        upstream: &mut Option<UpstreamSocket>,
        account_id: &mut String,
    ) -> Result<(), Value> {
        while pending.attempts < MAX_ATTEMPTS {
            pending.attempts += 1;
            if let Some(mut warm) = warmed.take()
                && warm.session == pending.session
                && warm.model == pending.model
                && pending.attempts == 1
            {
                pending.lease = warm.lease.take();
            }
            if pending.lease.is_none() {
                pending.lease = Some(LeaseGuard {
                    scheduler: self.scheduler.clone(),
                    lease: self
                        .scheduler
                        .acquire("codex", &pending.session, &pending.model, &pending.excluded)
                        .map_err(|error| unavailable_details(&error).0)?,
                });
            }
            let lease = &pending.lease.as_ref().expect("response lease").lease;
            if upstream.is_none() || *account_id != lease.account.id {
                *upstream = None;
                let credential = tokio::select! {
                    _ = self.cancel.cancelled() => return Err(error_details("proxy is shutting down")),
                    result = timeout(CALL_TIMEOUT,self.backend.credentials(&lease.account)) => match result {
                        Ok(Ok(credential)) if !credential.token.is_empty() => credential,
                        _ => { pending.excluded.insert(lease.account.id.clone()); pending.lease = None; continue; }
                    }
                };
                let address = endpoint(&lease.account, "/responses", query)
                    .map_err(|_| error_details("invalid upstream endpoint"))?;
                let headers = upstream_headers(headers, "codex", &credential)
                    .map_err(|_| error_details("invalid upstream credentials"))?;
                match self.dial_websocket(address, headers, &pending.model).await {
                    Ok(socket) => {
                        *upstream = Some(socket);
                        *account_id = lease.account.id.clone();
                    }
                    Err(HandshakeFailure::Rejected(rejection)) => {
                        self.reject(lease, rejection)
                            .map_err(|_| error_details("cannot persist account rejection"))?;
                        pending.excluded.insert(lease.account.id.clone());
                        pending.lease = None;
                        continue;
                    }
                    Err(HandshakeFailure::Other) => {
                        return Err(error_details(
                            "upstream WebSocket handshake failed; HTTP/SSE mode remains available",
                        ));
                    }
                }
            }
            let message = Message::Text(pending.body.clone().try_into().expect("validated UTF-8"));
            if !write_upstream(
                upstream.as_mut().expect("connected upstream"),
                message,
                &self.cancel,
            )
            .await
            {
                return Err(error_details("upstream WebSocket write failed"));
            }
            return Ok(());
        }
        Err(error_details("account retry limit reached"))
    }

    // Use the HTTP transport for handshakes so a rejected upgrade's body can be
    // read to the same bounded prefix as HTTP, even when it arrives after headers.
    async fn dial_websocket(
        &self,
        address: url::Url,
        mut headers: HeaderMap,
        model: &str,
    ) -> Result<UpstreamSocket, HandshakeFailure> {
        let key = generate_key();
        let accept = derive_accept_key(key.as_bytes());
        headers.insert("connection", HeaderValue::from_static("Upgrade"));
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
        headers.insert(
            "sec-websocket-key",
            HeaderValue::from_str(&key).expect("generated websocket key"),
        );
        let handshake = async {
            let mut response = self
                .client
                .get(address)
                .version(axum::http::Version::HTTP_11)
                .headers(headers)
                .send()
                .await
                .map_err(|_| HandshakeFailure::Other)?;
            if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let mut prefix = BytesMut::new();
                while prefix.len() < MAX_PRELUDE {
                    match response.chunk().await {
                        Ok(Some(bytes)) => prefix.extend_from_slice(
                            &bytes[..bytes.len().min(MAX_PRELUDE - prefix.len())],
                        ),
                        _ => break,
                    }
                }
                let (rejection, retry) = classify(status, &headers, &prefix, model);
                return Err(if retry {
                    HandshakeFailure::Rejected(rejection)
                } else {
                    HandshakeFailure::Other
                });
            }
            if !header(response.headers(), "upgrade").eq_ignore_ascii_case("websocket")
                || !header(response.headers(), "connection")
                    .split(',')
                    .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
                || header(response.headers(), "sec-websocket-accept") != accept
            {
                return Err(HandshakeFailure::Other);
            }
            let upgraded = response
                .upgrade()
                .await
                .map_err(|_| HandshakeFailure::Other)?;
            let config = WebSocketConfig::default()
                .read_buffer_size(8192)
                .write_buffer_size(8192)
                .max_message_size(Some(MAX_BODY))
                .max_frame_size(Some(MAX_BODY));
            Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await)
        };
        tokio::select! { _ = self.cancel.cancelled() => Err(HandshakeFailure::Other), result = timeout(Duration::from_secs(60),handshake) => result.unwrap_or(Err(HandshakeFailure::Other)) }
    }
}

fn error_details(message: &str) -> Value {
    json!({"type":"noria_error","message":message})
}
async fn write_error(socket: &mut WebSocket, cancel: &CancellationToken, message: &str) {
    write_details(socket, cancel, error_details(message)).await;
}
async fn write_details(socket: &mut WebSocket, cancel: &CancellationToken, details: Value) {
    let _ = write_client(
        socket,
        ClientMessage::Text(json!({"type":"error","error":details}).to_string().into()),
        cancel,
    )
    .await;
}
async fn write_client(
    socket: &mut WebSocket,
    message: ClientMessage,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! { _ = cancel.cancelled() => false, result = timeout(Duration::from_secs(60),socket.send(message)) => matches!(result,Ok(Ok(()))) }
}
async fn write_upstream(
    socket: &mut UpstreamSocket,
    message: Message,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! { _ = cancel.cancelled() => false, result = timeout(Duration::from_secs(60),socket.send(message)) => matches!(result,Ok(Ok(()))) }
}
