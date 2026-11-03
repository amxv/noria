mod cli;
mod dialogs;
mod menu;
mod model;
mod platform;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use model::{Account, ClientRoot, Dashboard, HealthReport, UpdateResult};
use notify::{RecursiveMode, Watcher};
use platform::EventSender;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{MenuEvent, MenuId},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
enum Action {
    Mutation(Vec<String>),
    Login(String, String),
    CancelLogin,
    Rename(Account),
    Remove(Account),
    Integrate(ClientRoot, bool),
    CustomIntegration,
    RefreshQuota,
    RefreshStatus,
    Health,
    CheckUpdate(bool),
    ApplyUpdate,
    Docs(&'static str),
    Quit,
}

impl Action {
    fn mutation<const N: usize>(args: [&str; N]) -> Self {
        Self::Mutation(args.map(str::to_owned).into())
    }
}

#[derive(Clone, Copy)]
enum Job {
    Status,
    Mutation,
    Quota,
    Login,
    Health,
    Update { automatic: bool, apply: bool },
    Sync,
    Reopen,
}

enum Event {
    Menu(MenuId),
    Refresh,
    Tick,
    Control,
    Command(Job, Result<Vec<u8>>, bool),
    Dialog(Result<Option<Action>>),
}

struct App {
    cli: cli::Cli,
    runtime: Runtime,
    sender: EventSender,
    tray: TrayIcon,
    actions: HashMap<MenuId, Action>,
    dashboard: Option<Dashboard>,
    status_error: Option<String>,
    status_busy: bool,
    mutation_busy: bool,
    quota_busy: bool,
    quota_error: bool,
    update_busy: bool,
    health_busy: bool,
    dialog_busy: bool,
    login: Option<CancellationToken>,
    shutdown: CancellationToken,
    next_status: Instant,
    next_quota: Instant,
    next_update: Instant,
}

impl App {
    fn command(&self, job: Job, args: Vec<String>, seconds: u64, cancel: CancellationToken) {
        let cli = self.cli.clone();
        let sender = self.sender.clone();
        self.runtime.spawn(async move {
            let result = cli.run(&args, seconds, cancel.clone()).await;
            sender.send(Event::Command(job, result, cancel.is_cancelled()));
        });
    }

    fn run<const N: usize>(&self, job: Job, args: [&str; N], seconds: u64) {
        self.command(
            job,
            args.map(str::to_owned).into(),
            seconds,
            self.shutdown.child_token(),
        );
    }

    fn render(&mut self) -> Result<()> {
        let (menu, actions) = menu::build(self)?;
        self.actions = actions;
        self.tray.set_menu(Some(Box::new(menu)));
        let running = self
            .dashboard
            .as_ref()
            .is_some_and(|dashboard| dashboard.running);
        self.tray.set_icon(Some(menu::icon(running)))?;
        self.tray.set_tooltip(Some(if self.dashboard.is_none() {
            "Noria · loading"
        } else if running {
            "Noria · running"
        } else {
            "Noria · stopped"
        }))?;
        Ok(())
    }

    fn load_status(&mut self) {
        if self.status_busy {
            return;
        }
        self.status_busy = true;
        self.next_status = Instant::now() + Duration::from_secs(30);
        self.run(Job::Status, ["status", "--json"], 8);
    }

    fn refresh_quota(&mut self) {
        if self.quota_busy {
            return;
        }
        self.quota_busy = true;
        self.quota_error = false;
        self.next_quota = Instant::now() + Duration::from_secs(300);
        self.run(Job::Quota, ["quota", "refresh"], 125);
    }

    fn check_update(&mut self, automatic: bool, apply: bool) {
        if self.update_busy || self.mutation_busy || self.login.is_some() {
            return;
        }
        self.update_busy = true;
        let job = Job::Update { automatic, apply };
        if apply {
            self.run(job, ["upgrade", "--json"], 185);
        } else {
            self.run(
                job,
                [
                    "upgrade",
                    if automatic { "--automatic" } else { "--check" },
                    "--json",
                ],
                185,
            );
        }
    }

    fn notice(&self, title: &'static str, error: impl ToString) {
        let detail = error.to_string().chars().take(245).collect::<String>();
        self.runtime
            .spawn_blocking(move || dialogs::notice(title, &detail));
    }

    fn dialog(&mut self, operation: impl FnOnce() -> Result<Option<Action>> + Send + 'static) {
        if self.dialog_busy {
            return;
        }
        self.dialog_busy = true;
        let sender = self.sender.clone();
        self.runtime
            .spawn_blocking(move || sender.send(Event::Dialog(operation())));
    }

    fn action(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => return false,
            Action::RefreshStatus => self.load_status(),
            Action::RefreshQuota => self.refresh_quota(),
            Action::Mutation(args) => {
                if !self.mutation_busy {
                    self.mutation_busy = true;
                    self.command(Job::Mutation, args, 55, self.shutdown.child_token());
                }
            }
            Action::Login(provider, id) => {
                if self.login.is_none() {
                    let token = self.shutdown.child_token();
                    let args = if id.is_empty() { vec!["account".into(), "add".into(), provider, "--json".into()] }
                        else { vec!["account".into(), "relogin".into(), id, "--json".into()] };
                    self.command(Job::Login, args, 310, token.clone());
                    self.login = Some(token);
                }
            }
            Action::CancelLogin => if let Some(login) = &self.login { login.cancel(); },
            Action::Rename(account) => self.dialog(move || {
                Ok(dialogs::prompt(&format!("Name for {} (blank uses email)", account.title()), &account.alias)?
                    .map(|alias| Action::mutation(["account", "rename", &account.id, &alias, "--json"])))
            }),
            Action::Remove(account) => self.dialog(move || {
                Ok(dialogs::confirm(&format!("Remove {}?", account.title()), "This unregisters the account. Its isolated sign-in files are retained; your normal client login is unchanged.")?
                    .then(|| Action::mutation(["account", "remove", &account.id, "--json"])))
            }),
            Action::Integrate(root, remove) => self.dialog(move || {
                let title = if remove { "Restore direct settings?".to_owned() } else { format!("Configure {} for Noria?", model::provider_name(&root.provider)) };
                let detail = format!("{}\n\nOnly routing settings change. Your login, conversation history and other preferences stay intact. Reopen already-running clients after this change.", root.root);
                if !dialogs::confirm(&title, &detail)? { return Ok(None); }
                let mut args = vec!["integrate".into(), root.provider, "--client-home".into(), root.root, "--yes".into(), "--json".into()];
                if remove { args.push("--remove".into()); }
                Ok(Some(Action::Mutation(args)))
            }),
            Action::CustomIntegration => self.dialog(|| {
                let Some(root) = dialogs::choose_folder()? else { return Ok(None); };
                Ok(dialogs::choose_provider()?.map(|provider| Action::Integrate(ClientRoot { provider, root }, false)))
            }),
            Action::Health => if !self.health_busy {
                self.health_busy = true;
                self.run(Job::Health, ["health", "--json"], 20);
            },
            Action::CheckUpdate(automatic) => self.check_update(automatic, false),
            Action::ApplyUpdate => self.check_update(false, true),
            Action::Docs(address) => if let Err(error) = dialogs::open_url(address) { self.notice("Could not open documentation", error); },
        }
        true
    }

    fn completed(&mut self, job: Job, result: Result<Vec<u8>>, canceled: bool) -> Result<bool> {
        match job {
            Job::Status => {
                self.status_busy = false;
                match result.and_then(|bytes| Dashboard::parse(&bytes)) {
                    Ok(dashboard) => {
                        if !dashboard.version.is_empty()
                            && dashboard.version != VERSION
                            && dashboard.version != "dev"
                        {
                            self.run(Job::Reopen, ["tray", "reopen"], 5);
                        }
                        self.status_error = None;
                        self.dashboard = Some(dashboard);
                    }
                    Err(_) => {
                        self.status_error =
                            Some("Status unavailable · Refresh Status or Health Check".into())
                    }
                }
            }
            Job::Mutation => {
                self.mutation_busy = false;
                if let Err(error) = result {
                    self.notice("Noria could not apply the change", error);
                }
                self.load_status();
            }
            Job::Quota => {
                self.quota_busy = false;
                self.quota_error = result.is_err();
                self.load_status();
            }
            Job::Login => {
                self.login = None;
                if !canceled && let Err(error) = result {
                    self.notice("Sign-in did not complete", error);
                }
                self.load_status();
                self.refresh_quota();
            }
            Job::Health => {
                self.health_busy = false;
                match result.and_then(|bytes| Ok(serde_json::from_slice::<HealthReport>(&bytes)?)) {
                    Ok(report) => {
                        let lines = report
                            .checks
                            .iter()
                            .map(|check| {
                                format!(
                                    "{} {}\n{}",
                                    if check.ok { "✓" } else { "•" },
                                    check.name,
                                    check.detail
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        self.runtime.spawn_blocking(move || {
                            dialogs::message(
                                "Noria Health Check",
                                &format!("{lines}\n\nNo model request was sent."),
                            )
                        });
                    }
                    Err(error) => self.notice("Health Check", error),
                }
            }
            Job::Update { automatic, apply } => {
                self.update_busy = false;
                match result {
                    Ok(bytes) if bytes.is_empty() => (),
                    Ok(bytes) => match serde_json::from_slice::<UpdateResult>(&bytes) {
                        Ok(update) if update.updated => self.load_status(),
                        Ok(update) if apply => if !update.message.is_empty() { self.notice("Noria Update", update.message); },
                        Ok(update) if !automatic && update.available => self.dialog(move || {
                            Ok(dialogs::confirm(&format!("Noria {} is available", update.latest), "The CLI verifies checksums and waits for active requests to finish before upgrading.")?.then_some(Action::ApplyUpdate))
                        }),
                        Ok(update) if !automatic => {
                            self.runtime.spawn_blocking(move || dialogs::message("Noria is up to date", &format!("Version {}", update.current)));
                        }
                        Err(error) if !automatic => self.notice("Update check failed", error),
                        _ => (),
                    },
                    Err(error) if !automatic => self.notice("Update check failed", error),
                    Err(_) => (),
                }
            }
            Job::Reopen => {
                if result.is_ok() {
                    return Ok(false);
                }
            }
            Job::Sync => (),
        }
        Ok(true)
    }

    fn event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Menu(id) => {
                if let Some(action) = self.actions.get(&id).cloned()
                    && !self.action(action)
                {
                    return Ok(false);
                }
            }
            Event::Refresh => self.load_status(),
            Event::Control => {
                if take_quit_request(&self.cli.home) {
                    return Ok(false);
                }
            }
            Event::Command(job, result, canceled) => {
                if !self.completed(job, result, canceled)? {
                    return Ok(false);
                }
            }
            Event::Dialog(result) => {
                self.dialog_busy = false;
                match result {
                    Ok(Some(action)) => {
                        if !self.action(action) {
                            return Ok(false);
                        }
                    }
                    Err(error) => self.notice("Desktop dialog unavailable", error),
                    Ok(None) => (),
                }
            }
            Event::Tick => (),
        }
        let now = Instant::now();
        if now >= self.next_status {
            self.load_status();
        }
        if now >= self.next_quota {
            self.refresh_quota();
        }
        if now >= self.next_update {
            self.next_update = now + Duration::from_secs(3600);
            self.check_update(true, false);
        }
        self.render()?;
        Ok(true)
    }
}

fn take_quit_request(home: &Path) -> bool {
    #[derive(Deserialize)]
    struct Control {
        action: String,
        requested_at: DateTime<Utc>,
    }
    let path = home.join("tray-control.json");
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    let Ok(command) = serde_json::from_slice::<Control>(&bytes) else {
        return false;
    };
    let _ = fs::remove_file(path);
    let age = Utc::now() - command.requested_at;
    command.action == "quit"
        && age > chrono::Duration::seconds(-60)
        && age < chrono::Duration::seconds(120)
}

fn acquire_lock(home: &Path) -> Result<Option<File>> {
    let mut directory = fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(home)?;
    let path = home.join("tray.lock");
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
        bail!("invalid tray lock file");
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.set_len(0)?;
            writeln!(file, "pid={}", std::process::id())?;
            Ok(Some(file))
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

pub fn main() -> Result<()> {
    let mut home = None;
    let mut check = false;
    let mut check_host = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--home" => {
                home = Some(PathBuf::from(
                    args.next().context("--home needs a directory")?,
                ))
            }
            "--check" => check = true,
            "--check-host" => check_host = true,
            "--help" | "-h" => {
                println!("Usage: noria-tray [--home DIR] [--check | --check-host]");
                return Ok(());
            }
            _ if arg.starts_with("--home=") => home = Some(PathBuf::from(&arg[7..])),
            _ => bail!("unexpected tray argument: {arg}"),
        }
    }
    if check {
        println!("noria-tray {VERSION} ready");
        return Ok(());
    }
    if check_host {
        platform::check_host()?;
        println!("Desktop status icon host available");
        return Ok(());
    }
    platform::prepare()?;
    let cli = cli::Cli::new(home)?;
    let Some(_lock) = acquire_lock(&cli.home)? else {
        return Ok(());
    };
    let _ = fs::remove_file(cli.home.join("tray-control.json"));
    let (sender, receiver) = platform::channel();
    let callback = sender.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        callback.send(Event::Menu(event.id))
    }));
    let callback = sender.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick { .. }
        ) {
            callback.send(Event::Refresh);
        }
    }));
    let callback = sender.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if result.is_ok_and(|event| {
            matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            ) && event.paths.iter().any(|path| {
                path.file_name()
                    .is_some_and(|name| name == "tray-control.json")
            })
        }) {
            callback.send(Event::Control);
        }
    })?;
    watcher.watch(&cli.home, RecursiveMode::NonRecursive)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(4)
        .enable_all()
        .build()?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("Noria · loading")
        .with_icon(menu::icon(false))
        .with_menu(Box::new(tray_icon::menu::Menu::new()))
        .build()?;
    let now = Instant::now();
    let mut app = App {
        cli,
        runtime,
        sender,
        tray,
        actions: HashMap::new(),
        dashboard: None,
        status_error: None,
        status_busy: false,
        mutation_busy: false,
        quota_busy: false,
        quota_error: false,
        update_busy: false,
        health_busy: false,
        dialog_busy: false,
        login: None,
        shutdown: CancellationToken::new(),
        next_status: now,
        next_quota: now + Duration::from_secs(300),
        next_update: now + Duration::from_secs(30),
    };
    app.render()?;
    app.load_status();
    app.run(Job::Sync, ["__sync-updates"], 20);
    let mut failure = None;
    let result = platform::run(receiver, |event| match app.event(event) {
        Ok(keep_running) => keep_running,
        Err(error) => {
            failure = Some(error);
            false
        }
    });
    app.shutdown.cancel();
    drop(watcher);
    drop(app.tray);
    app.runtime.shutdown_timeout(Duration::from_secs(2));
    result?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_file_is_fresh_and_consumed() {
        let directory = tempfile::tempdir().unwrap();
        for (seconds, expected) in [(0, true), (-121, false), (61, false)] {
            let requested = Utc::now() + chrono::Duration::seconds(seconds);
            fs::write(
                directory.path().join("tray-control.json"),
                serde_json::json!({"action":"quit", "requested_at":requested}).to_string(),
            )
            .unwrap();
            assert_eq!(take_quit_request(directory.path()), expected);
            assert!(!directory.path().join("tray-control.json").exists());
        }
    }

    #[test]
    fn lock_survives_stale_file_but_excludes_second_instance() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("tray.lock"), "pid=999999999").unwrap();
        let first = acquire_lock(directory.path()).unwrap().unwrap();
        assert!(acquire_lock(directory.path()).unwrap().is_none());
        drop(first);
        assert!(acquire_lock(directory.path()).unwrap().is_some());
    }
}
