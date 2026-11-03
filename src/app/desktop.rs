use super::*;
use crate::distribution;
use chrono::Utc;
use std::{collections::BTreeMap, ffi::OsString, fs, process::Stdio};
use tokio::process::Command;

pub(super) fn tray_binary(exe: &Path) -> PathBuf {
    exe.parent()
        .unwrap_or(Path::new("."))
        .join(distribution::tray_name())
}
pub(super) fn menu_path(exe: &Path) -> PathBuf {
    exe.parent().unwrap_or(Path::new(".")).join("Noria.app")
}

pub async fn preferences(home: &Path, mut args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!(
            "Usage: noria preferences [start-at-login|menubar-at-login|tray-at-login|auto-update on|off] [--json]\nAll preferences are owned by the CLI. Quitting the desktop UI does not stop routing."
        );
        return Ok(());
    }
    let as_json = flag(&mut args, "--json");
    config::init(home)?;
    if !args.is_empty() {
        ensure!(
            args.len() == 2 && matches!(args[1].as_str(), "on" | "off"),
            "specify a preference followed by on or off"
        );
        let enabled = args[1] == "on";
        let exe = service::executable()?;
        match args[0].as_str() {
            "start-at-login" => service::login(home, &exe, enabled).await?,
            "menubar-at-login" => menu_login(home, &exe, enabled)?,
            "tray-at-login" => tray_login(home, &exe, enabled).await?,
            "auto-update" => {}
            _ => bail!("unknown preference"),
        }
        config::mutate(home, |c| {
            match args[0].as_str() {
                "start-at-login" => c.start_at_login = enabled,
                "menubar-at-login" | "tray-at-login" => c.menubar_at_login = enabled,
                "auto-update" => c.auto_update = enabled,
                _ => unreachable!(),
            }
            Ok(())
        })?;
        if args[0] == "auto-update" {
            sync_updates(home, &exe)
                .await
                .context("automatic update scheduling failed")?;
        }
    }
    let c = config::load(home)?;
    let mut value = json!({"start_at_login":c.start_at_login,"menubar_at_login":c.menubar_at_login,"auto_update":c.auto_update});
    if !cfg!(target_os = "macos") {
        value["tray_at_login"] = json!(c.menubar_at_login);
    }
    if as_json {
        return print_json(&value);
    }
    println!(
        "Start at login: {}\n{} at login: {}\nAutomatic updates: {}",
        c.start_at_login,
        if cfg!(target_os = "macos") {
            "Menu bar"
        } else {
            "Tray"
        },
        c.menubar_at_login,
        c.auto_update
    );
    Ok(())
}

pub(super) fn tray_environment() -> Result<BTreeMap<OsString, OsString>> {
    #[allow(unused_mut)]
    let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
    #[cfg(target_os = "linux")]
    {
        ensure!(
            env.get(&OsString::from("DISPLAY"))
                .is_some_and(|v| !v.is_empty())
                || env
                    .get(&OsString::from("WAYLAND_DISPLAY"))
                    .is_some_and(|v| !v.is_empty()),
            "a graphical Linux desktop session is required for the tray"
        );
        if !env
            .get(&OsString::from("DBUS_SESSION_BUS_ADDRESS"))
            .is_some_and(|v| !v.is_empty())
        {
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .context("a desktop session D-Bus connection is required to show the tray")?;
            let bus = dir.join("bus");
            let info=fs::metadata(&bus).context("the user D-Bus session socket is unavailable; start Noria from your desktop session")?;
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                info.file_type().is_socket(),
                "the user D-Bus session socket is unavailable"
            );
            env.insert(
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={}", bus.to_string_lossy().replace(',', "%2C")).into(),
            );
        }
    }
    Ok(env)
}
pub async fn tray(home: &Path, args: Vec<String>) -> Result<()> {
    if args.is_empty() || help_requested(&args) {
        println!(
            "Usage: noria tray open|quit|reopen\nWindows/Linux desktop tray wrapper. Quitting the tray does not stop the Noria router."
        );
        return Ok(());
    }
    ensure!(!cfg!(target_os = "macos"), "use noria menubar on macOS");
    ensure!(args.len() == 1, "usage: noria tray open|quit|reopen");
    config::init(home)?;
    let exe = service::executable()?;
    match args[0].as_str() {
        "open" => {
            let env = tray_environment()?;
            ensure_tray(home, &exe).await?;
            let binary = tray_binary(&exe);
            let mut check = Command::new(&binary);
            check
                .arg("--check-host")
                .env_clear()
                .envs(&env)
                .kill_on_drop(true);
            match tokio::time::timeout(Duration::from_secs(4), check.output()).await {
                Ok(Ok(result)) if result.status.success() => {}
                _ => eprintln!(
                    "Desktop tray host not ready; the icon will appear when the desktop's status icon service is available."
                ),
            }
            let mut command = Command::new(binary);
            command
                .args(["--home"])
                .arg(home)
                .current_dir(home)
                .env_clear()
                .envs(env)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            service::detached(&mut command);
            drop(command.spawn()?);
            println!("Started Noria tray");
        }
        "quit" => match config::lock_timeout(home, "tray", Duration::from_millis(150)) {
            Ok(guard) => {
                drop(guard);
                println!("Noria tray is not running (router unchanged)");
            }
            Err(error) if error.to_string().contains("busy") => {
                request_tray_quit(home)?;
                println!("Requested Noria tray exit (router keeps running)");
            }
            Err(error) => return Err(error),
        },
        "reopen" => {
            let mut command = Command::new(exe);
            command
                .args(["__tray-reopen", "--home"])
                .arg(home)
                .current_dir(home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            service::detached(&mut command);
            drop(command.spawn()?);
        }
        _ => bail!("unknown tray command"),
    }
    Ok(())
}
pub(super) fn request_tray_quit(home: &Path) -> Result<()> {
    config::write(
        home.join("tray-control.json"),
        &serde_json::to_vec(&json!({"action":"quit","requested_at":Utc::now()}))?,
    )
}
pub async fn delayed_tray(home: &Path, args: Vec<String>) -> Result<()> {
    ensure!(
        !cfg!(target_os = "macos") && args.is_empty(),
        "invalid tray relaunch"
    );
    drop(config::lock_timeout(home, "tray", Duration::from_secs(45))?);
    tray(home, vec!["open".into()]).await
}
async fn ensure_tray(home: &Path, exe: &Path) -> Result<()> {
    let _lock = config::lock_timeout(home, "tray-install", Duration::from_secs(180))?;
    let target = tray_binary(exe);
    match fs::symlink_metadata(&target) {
        Ok(st) => {
            ensure!(
                st.is_file() && !st.file_type().is_symlink(),
                "Noria tray path is not a regular file"
            );
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    ensure!(
        distribution::newer(crate::VERSION, "0.0.0"),
        "tray executable is missing; build the companion noria-tray binary"
    );
    let release = distribution::tagged(crate::VERSION)?;
    let temp = tempfile::Builder::new()
        .prefix(".tray-install-")
        .tempdir_in(home)?;
    let archive = temp.path().join("release.tar.gz");
    distribution::download(&release, &distribution::archive_name(), &archive).await?;
    let unpacked = temp.path().join("unpacked");
    distribution::extract(&archive, &unpacked)?;
    let source = unpacked.join(distribution::tray_name());
    distribution::executable_mode(&source)?;
    let mut probe = Command::new(&source);
    probe.arg("--check").kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), probe.output()).await??;
    ensure!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim()
                == format!("noria-tray {} ready", crate::VERSION),
        "downloaded tray failed version verification"
    );
    distribution::swap(&source, &target)?.finish();
    Ok(())
}

pub async fn menubar(home: &Path, mut args: Vec<String>) -> Result<()> {
    if args.is_empty() || help_requested(&args) {
        println!(
            "Usage: noria menubar install [--source APP_PATH]\n       noria menubar open\n       noria menubar reopen [--wait-pid PID]\nmacOS native wrapper. Preferences and accounts are owned by the CLI."
        );
        return Ok(());
    }
    ensure!(
        cfg!(target_os = "macos"),
        "the native menu bar app is available on macOS; CLI start/stop works on every platform"
    );
    config::init(home)?;
    let exe = service::executable()?;
    let target = menu_path(&exe);
    let op = args.remove(0);
    match op.as_str() {
        "install" => {
            let source = option(&mut args, "--source")?;
            no_args(&args, "noria menubar install [--source APP_PATH]")?;
            let temp = tempfile::Builder::new()
                .prefix(".menubar-")
                .tempdir_in(home)?;
            let source = match source {
                Some(p) => PathBuf::from(p),
                None => {
                    let archive = temp.path().join("app.tar.gz");
                    distribution::download(
                        &distribution::tagged(crate::VERSION)?,
                        &distribution::archive_name(),
                        &archive,
                    )
                    .await?;
                    let unpacked = temp.path().join("app");
                    distribution::extract(&archive, &unpacked)?;
                    unpacked.join("Noria.app")
                }
            };
            update::verify_app(&source).await?;
            update::replace_app(&source, &target).await?;
            println!("Installed {}", target.display());
        }
        "open" => {
            no_args(&args, "noria menubar open")?;
            ensure!(
                target.is_dir(),
                "menu bar app is not installed; run noria menubar install"
            );
            if let Err(error) = sync_updates(home, &exe).await {
                eprintln!("Automatic updates need attention: {error}");
            }
            service::run(
                "/usr/bin/open",
                &[
                    target.to_string_lossy().into_owned(),
                    "--args".into(),
                    "--home".into(),
                    home.to_string_lossy().into_owned(),
                ],
            )
            .await?;
        }
        "reopen" => {
            let pid = option(&mut args, "--wait-pid")?.unwrap_or_else(|| "0".into());
            let _: u32 = pid.parse().context("invalid menu bar process ID")?;
            no_args(&args, "noria menubar reopen [--wait-pid PID]")?;
            let mut command = Command::new(exe);
            command
                .args(["__menubar-reopen", "--wait-pid", &pid, "--home"])
                .arg(home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            service::detached(&mut command);
            drop(command.spawn()?);
        }
        _ => bail!("unknown menubar command"),
    }
    Ok(())
}
pub async fn delayed_menu(home: &Path, mut args: Vec<String>) -> Result<()> {
    let pid = option(&mut args, "--wait-pid")?
        .context("invalid menu bar relaunch request")?
        .parse::<u32>()
        .context("invalid menu bar process ID")?;
    no_args(&args, "internal menu bar relaunch")?;
    if pid == 0 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    } else {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while service::run("/bin/kill", &["-0".into(), pid.to_string()])
            .await
            .is_ok()
        {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "previous menu bar instance did not exit"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    menubar(home, vec!["open".into()]).await
}
fn menu_login(home: &Path, exe: &Path, enabled: bool) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "the native menu bar is available on macOS"
    );
    let path = service::user_home()?
        .join("Library/LaunchAgents")
        .join(format!("{}.menu.plist", service::label(home)));
    if !enabled {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    ensure!(
        menu_path(exe).is_dir(),
        "install the menu bar app before enabling login startup"
    );
    let contents = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}.menu</string><key>ProgramArguments</key><array><string>{}</string><string>menubar</string><string>open</string><string>--home</string><string>{}</string></array><key>RunAtLoad</key><true/></dict></plist>",
        service::label(home),
        service::xml(&exe.to_string_lossy()),
        service::xml(&home.to_string_lossy())
    );
    service::user_file(&path, contents.as_bytes())
}
#[cfg(any(target_os = "linux", test))]
fn desktop_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('%', "%%")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$")
    )
}
async fn tray_login(home: &Path, exe: &Path, enabled: bool) -> Result<()> {
    ensure!(
        !cfg!(target_os = "macos"),
        "the native tray app is available on Windows and Linux"
    );
    let tray = tray_binary(exe);
    if enabled {
        ensure!(
            tray.is_file(),
            "install Noria tray before enabling login startup"
        );
    }
    #[cfg(target_os = "linux")]
    {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or(service::user_home()?.join(".config"));
        ensure!(xdg.is_absolute(), "XDG_CONFIG_HOME must be absolute");
        let path = xdg.join("autostart/noria-tray.desktop");
        if !enabled {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        ensure!(
            !tray.to_string_lossy().contains(['\n', '\r'])
                && !home.to_string_lossy().contains(['\n', '\r']),
            "tray startup path contains unsupported characters"
        );
        let contents = format!(
            "[Desktop Entry]\nType=Application\nName=Noria Tray\nComment=Local account router\nExec={} --home {}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            desktop_quote(&tray.to_string_lossy()),
            desktop_quote(&home.to_string_lossy())
        );
        service::user_file(&path, contents.as_bytes())?;
    }
    #[cfg(windows)]
    {
        let key = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
        if !enabled {
            let _ = service::run(
                "reg.exe",
                &[
                    "delete".into(),
                    key.into(),
                    "/v".into(),
                    "Noria Tray".into(),
                    "/f".into(),
                ],
            )
            .await;
            return Ok(());
        }
        let paths = format!("{}{}", tray.display(), home.display());
        ensure!(
            !paths.contains(['"', '\n', '\r']),
            "tray startup path contains unsupported characters"
        );
        service::run(
            "reg.exe",
            &[
                "add".into(),
                key.into(),
                "/v".into(),
                "Noria Tray".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                format!("\"{}\" --home \"{}\"", tray.display(), home.display()),
                "/f".into(),
            ],
        )
        .await?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn update_plist(home: &Path, exe: &Path, c: &config::Config) -> Result<String> {
    let esc = service::xml;
    let log = esc(&home.join("logs/updates.log").to_string_lossy());
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}.update</string><key>ProgramArguments</key><array><string>{}</string><string>upgrade</string><string>--automatic</string><string>--json</string><string>--home</string><string>{}</string></array><key>RunAtLoad</key><true/><key>StartInterval</key><integer>3600</integer><key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string><key>NORIA_HOME</key><string>{}</string><key>PATH</key><string>{}</string></dict><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string></dict></plist>",
        service::label(home),
        esc(&exe.to_string_lossy()),
        esc(&home.to_string_lossy()),
        esc(&service::user_home()?.to_string_lossy()),
        esc(&home.to_string_lossy()),
        esc(&c.tool_path)
    ))
}
pub async fn sync_updates(home: &Path, exe: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let c = config::load(home)?;
        let label = format!("{}.update", service::label(home));
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let job = format!("{domain}/{label}");
        let path = service::user_home()?
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        let loaded = service::run("/bin/launchctl", &["print".into(), job.clone()])
            .await
            .is_ok();
        if !c.auto_update {
            if path.exists() {
                fs::remove_file(path)?;
            }
            if loaded {
                if let Ok(_guard) = config::lock(home, "upgrade") {
                    service::run("/bin/launchctl", &["bootout".into(), job]).await?;
                }
            }
            return Ok(());
        }
        ensure!(
            exe.is_absolute(),
            "updater executable path must be absolute"
        );
        config::private_dir(home.join("logs"))?;
        service::user_file(&path, update_plist(home, exe, &c)?.as_bytes())?;
        if !loaded
            && service::run("/bin/launchctl", &["print".into(), domain.clone()])
                .await
                .is_ok()
        {
            service::run(
                "/bin/launchctl",
                &[
                    "bootstrap".into(),
                    domain,
                    path.to_string_lossy().into_owned(),
                ],
            )
            .await?;
        }
    }
    let _ = (home, exe);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_entry_escaping_is_not_shell_interpolation() {
        assert_eq!(desktop_quote("a%$`\""), "\"a%%\\$\\`\\\"\"");
    }
}
