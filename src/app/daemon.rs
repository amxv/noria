use super::*;
use crate::{core::Scheduler, model::Account, provider::Manager, proxy::Server};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::{StreamExt, stream};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use serde::{Deserialize, Serialize};
use std::{process::Stdio, sync::Arc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefreshResult {
    pub account: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}
struct Refresher {
    home: PathBuf,
    scheduler: Arc<Scheduler>,
    provider: Arc<Manager>,
    lock: tokio::sync::Mutex<()>,
}
impl Refresher {
    async fn refresh(&self, id: &str) -> Result<Vec<RefreshResult>> {
        let _guard = self.lock.lock().await;
        let c = config::load(&self.home)?;
        if !id.is_empty() {
            accounts::find(&c, id)?;
        }
        self.scheduler.set_accounts(c.accounts.clone())?;
        let accounts: Vec<Account> = c
            .accounts
            .into_iter()
            .filter(|a| {
                if !id.is_empty() {
                    a.id == id
                } else {
                    a.enabled || a.authenticated
                }
            })
            .collect();
        let results = stream::iter(accounts.into_iter().map(|a| async move {
            let outcome = tokio::time::timeout(Duration::from_secs(25), async {
                if a.email.is_empty()
                    && let Ok(identity) = self.provider.identity(&a).await
                    && identity.authenticated
                    && !identity.email.is_empty()
                {
                    let _ = config::mutate(&self.home, |c| {
                        if let Some(v) = c.accounts.iter_mut().find(|v| v.id == a.id) {
                            v.email = identity.email;
                            v.authenticated = true;
                        }
                        Ok(())
                    });
                }
                self.scheduler.update(self.provider.refresh(&a).await?)
            })
            .await;
            match outcome {
                Ok(Ok(())) => RefreshResult {
                    account: a.id,
                    ok: true,
                    error: String::new(),
                },
                Ok(Err(error)) => RefreshResult {
                    account: a.id,
                    ok: false,
                    error: error.to_string(),
                },
                Err(_) => RefreshResult {
                    account: a.id,
                    ok: false,
                    error: "refresh timed out".into(),
                },
            }
        }))
        .buffered(4)
        .collect()
        .await;
        if let Ok(c) = config::load(&self.home) {
            self.scheduler.set_accounts(c.accounts)?;
        }
        Ok(results)
    }
}
#[derive(Clone)]
struct Control {
    gateway: Arc<Server>,
    refresher: Arc<Refresher>,
    cancel: CancellationToken,
}
async fn control_handler(State(state): State<Control>, request: Request<Body>) -> Response {
    if let Err(response) = state.gateway.authorize(request.headers()) {
        return response;
    }
    if request.method() != reqwest::Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if !request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer "))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match request.uri().path() {
        "/_noria/shutdown" => {
            let cancel = state.cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel.cancel();
            });
            Json(json!({"ok":true})).into_response()
        }
        "/_noria/reload" => {
            match config::load(&state.refresher.home)
                .and_then(|c| state.refresher.scheduler.set_accounts(c.accounts))
            {
                Ok(()) => Json(json!({"ok":true})).into_response(),
                Err(_) => (StatusCode::BAD_REQUEST, "cannot reload accounts").into_response(),
            }
        }
        "/_noria/refresh" => {
            let bytes = match to_bytes(request.into_body(), 4096).await {
                Ok(b) => b,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "invalid refresh request").into_response();
                }
            };
            #[derive(Deserialize)]
            struct Payload {
                #[serde(default)]
                account: String,
            }
            let payload: Payload = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "invalid refresh request").into_response();
                }
            };
            match state.refresher.refresh(&payload.account).await {
                Ok(v) => Json(v).into_response(),
                Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn serve(home: &Path) -> Result<()> {
    let c = config::load(home)?;
    config::validate_listen(&c.listen)?;
    let _guard = config::lock(home, "daemon")?;
    let scheduler = Arc::new(Scheduler::new(home.join("scheduler.json"), c.accounts)?);
    let provider = Arc::new(Manager::new(home));
    let gateway = Arc::new(Server::new(
        scheduler.clone(),
        provider.clone(),
        config::secret(home)?,
    ));
    let refresher = Arc::new(Refresher {
        home: home.to_owned(),
        scheduler,
        provider,
        lock: tokio::sync::Mutex::new(()),
    });
    let cancel = CancellationToken::new();
    let state = Control {
        gateway: gateway.clone(),
        refresher: refresher.clone(),
        cancel: cancel.clone(),
    };
    let router = gateway.router().merge(
        Router::new()
            .route("/_noria/refresh", any(control_handler))
            .route("/_noria/reload", any(control_handler))
            .route("/_noria/shutdown", any(control_handler))
            .with_state(state),
    );
    let listener = tokio::net::TcpListener::bind(&c.listen).await?;
    let address = listener.local_addr()?.to_string();
    config::write(
        home.join("daemon.json"),
        &serde_json::to_vec(&json!({"address":address}))?,
    )?;
    struct RuntimeFile(PathBuf);
    impl Drop for RuntimeFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _runtime_file = RuntimeFile(home.join("daemon.json"));
    let exe = service::executable()?;
    if service::read_intent(home).wanted || c.start_at_login {
        service::set_intent(home, &exe, true)?;
        if let Err(error) = integration::set_active(home, &exe, &address, true) {
            eprintln!("Client integration needs attention: {error}");
        }
    }
    let workers = TaskTracker::new();
    let refresh_cancel = cancel.clone();
    workers.spawn(async move {
        let mut tick=tokio::time::interval(Duration::from_secs(60));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop{tokio::select!{_=refresh_cancel.cancelled()=>break,_=tick.tick()=>{tokio::select!{_=refresh_cancel.cancelled()=>break,_=refresher.refresh("")=>{}}}}}
    });
    if service::read_intent(home).wanted {
        workers.spawn(automatic_updates(home.to_owned(), cancel.clone()));
    }
    let signal_cancel = cancel.clone();
    workers.spawn(async move {
        tokio::select! {_=signal_cancel.cancelled()=>{},_=shutdown_signal()=>signal_cancel.cancel()}
    });
    println!("Noria listening on http://{address}");
    let connections = TaskTracker::new();
    let slots = Arc::new(tokio::sync::Semaphore::new(128));
    loop {
        let permit = tokio::select! {_=cancel.cancelled()=>break,permit=slots.clone().acquire_owned()=>permit?};
        let accepted = tokio::select! {_=cancel.cancelled()=>break,value=listener.accept()=>value};
        let (stream, _) = match accepted {
            Ok(value) => value,
            Err(error) => {
                drop(permit);
                eprintln!("Local connection could not be accepted: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let router = router.clone();
        let connection_cancel = cancel.clone();
        connections.spawn(async move {
            let _permit=permit;
            let mut builder=Builder::new(TokioExecutor::new());
            builder.http1().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).max_buf_size(64<<10);
            builder.http2().max_header_list_size(64<<10);
            let connection=builder.serve_connection_with_upgrades(TokioIo::new(stream),TowerToHyperService::new(router));
            tokio::pin!(connection);
            tokio::select!{
                _=&mut connection=>{},
                _=connection_cancel.cancelled()=>{connection.as_mut().graceful_shutdown();let _=tokio::time::timeout(Duration::from_secs(10),&mut connection).await;}
            }
        });
    }
    cancel.cancel();
    drop(listener);
    connections.close();
    workers.close();
    let _ = tokio::time::timeout(Duration::from_secs(10), gateway.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(11), async {
        tokio::join!(connections.wait(), workers.wait());
    })
    .await;
    Ok(())
}
pub(super) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
async fn automatic_updates(home: PathBuf, cancel: CancellationToken) {
    let mut delay = Duration::from_secs(120);
    loop {
        tokio::select! {_=cancel.cancelled()=>break,_=tokio::time::sleep(delay)=>{}}
        delay = Duration::from_secs(15 * 60);
        if config::load(&home).is_ok_and(|c| c.auto_update) {
            let intent = service::read_intent(&home);
            if !intent.executable.is_empty() {
                let mut command = tokio::process::Command::new(intent.executable);
                command
                    .args(["upgrade", "--automatic", "--json", "--home"])
                    .arg(&home)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                service::detached(&mut command);
                if let Ok(mut child) = command.spawn() {
                    tokio::select! {_=cancel.cancelled()=>break,_=child.wait()=>{}}
                }
            }
        }
    }
}
pub(super) async fn refresh_account(home: &Path, id: &str) -> Result<Vec<RefreshResult>> {
    if let Ok(value) = control(
        home,
        reqwest::Method::POST,
        "/_noria/refresh",
        Some(&json!({"account":id})),
    )
    .await
    {
        return Ok(serde_json::from_value(value)?);
    }
    let _guard =
        config::lock(home, "daemon").context("live refresh failed and scheduler is locked")?;
    let c = config::load(home)?;
    let refresher = Refresher {
        home: home.to_owned(),
        scheduler: Arc::new(Scheduler::new(home.join("scheduler.json"), c.accounts)?),
        provider: Arc::new(Manager::new(home)),
        lock: tokio::sync::Mutex::new(()),
    };
    refresher.refresh(id).await
}
pub async fn quota(home: &Path, args: Vec<String>) -> Result<()> {
    if args.is_empty() || help_requested(&args) {
        println!(
            "Usage: noria quota refresh [account]\nRead native provider usage with isolated auth; sends no user prompt and spends no reset credits."
        );
        return Ok(());
    }
    ensure!(
        args[0] == "refresh" && args.len() <= 2,
        "usage: noria quota refresh [account]"
    );
    let id = if let Some(reference) = args.get(1) {
        accounts::resolve(&config::load(home)?, reference)?
            .id
            .clone()
    } else {
        String::new()
    };
    let result = refresh_account(home, &id).await?;
    print_json(&result)?;
    ensure!(
        result.iter().all(|r| r.ok),
        "one or more native quota reads failed; inspect per-account results"
    );
    Ok(())
}
