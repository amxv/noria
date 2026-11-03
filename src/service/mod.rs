//! User-scoped background lifetime, independent of any desktop UI.
mod platform;

use crate::config::{self, Config};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use notify::Watcher;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Intent {
    #[serde(default)]
    pub wanted: bool,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

pub fn label(home: &Path) -> String {
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    format!(
        "xyz.ashray.noria.{}",
        digest[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

pub fn executable() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

pub fn user_home() -> Result<PathBuf> {
    let value = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    };
    value
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("user home is unavailable")
}

pub fn set_intent(home: &Path, exe: &Path, wanted: bool) -> Result<()> {
    config::write(
        home.join("service.json"),
        &serde_json::to_vec(&Intent {
            wanted,
            executable: exe.to_string_lossy().into_owned(),
            updated_at: Some(Utc::now()),
        })?,
    )
}

pub fn read_intent(home: &Path) -> Intent {
    fs::read(home.join("service.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub async fn start(home: &Path, exe: &Path) -> Result<()> {
    set_intent(home, exe, true)?;
    let mut c = config::load(home)?;
    if c.tool_path.is_empty() {
        c.tool_path = std::env::var("PATH").unwrap_or_default();
        config::mutate(home, |v| {
            v.tool_path.clone_from(&c.tool_path);
            Ok(())
        })?;
    }
    if let Err(error) = platform::start(home, exe, &c).await {
        let _ = set_intent(home, exe, false);
        return Err(error);
    }
    Ok(())
}

pub async fn stop(home: &Path, exe: &Path) -> Result<()> {
    set_intent(home, exe, false)?;
    platform::stop(home).await
}

pub async fn login(home: &Path, exe: &Path, enabled: bool) -> Result<()> {
    platform::login(home, exe, enabled).await
}

pub fn environment(home: &Path, c: &Config) -> Result<BTreeMap<OsString, OsString>> {
    let mut env = BTreeMap::new();
    for key in [
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(v) = std::env::var_os(key) {
            env.insert(key.into(), v);
        }
    }
    env.insert("HOME".into(), user_home()?.into_os_string());
    env.insert("NORIA_HOME".into(), home.as_os_str().to_owned());
    env.insert("PATH".into(), c.tool_path.clone().into());
    Ok(env)
}

pub fn log_file(home: &Path, name: &str) -> Result<fs::File> {
    let dir = home.join("logs");
    config::private_dir(&dir)?;
    let path = dir.join(name);
    if let Ok(info) = fs::symlink_metadata(&path) {
        if !info.is_file() || info.file_type().is_symlink() {
            bail!("refusing nonregular log file");
        }
        if info.len() > 2 << 20 {
            let _ = fs::rename(&path, dir.join(format!("{name}.1")));
        }
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

pub fn detached(command: &mut Command) {
    #[cfg(unix)]
    {
        // SAFETY: setsid is async-signal-safe and touches no inherited locks.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000 | 0x00000200);
    }
}

pub async fn detach(home: &Path, exe: &Path, c: &Config) -> Result<()> {
    let log = log_file(home, "service.log")?;
    let mut command = Command::new(exe);
    command
        .args(["__supervise", "--home"])
        .arg(home)
        .env_clear()
        .envs(environment(home, c)?)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detached(&mut command);
    let child = command.spawn().context("start user supervisor")?;
    // Dropping a Tokio Child does not kill it. It is owned by the user session.
    drop(child);
    Ok(())
}

pub async fn supervise(home: &Path) -> Result<()> {
    let _guard = config::lock(home, "supervisor")?;
    let exe = executable()?;
    // Config writes are atomic renames; watch the containing directory. A
    // capacity-one wakeup coalesces updates while never polling the filesystem.
    let (_watcher, mut changes) = watch_intent(home)?;
    while read_intent(home).wanted {
        let c = config::load(home)?;
        let mut command = Command::new(&exe);
        command
            .args(["__daemon", "--home"])
            .arg(home)
            .env_clear()
            .envs(environment(home, &c)?)
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        detached(&mut command);
        let mut child = command.spawn()?;
        let status = loop {
            tokio::select! {
                status = child.wait() => break status?,
                changed = changes.recv() => {
                    if changed.is_none() || !read_intent(home).wanted {
                        if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
                            kill_child(&mut child).await;
                        }
                        return Ok(());
                    }
                }
            }
        };
        if !read_intent(home).wanted {
            break;
        }
        if !status.success() {
            eprintln!("Noria service restarting after an unexpected exit");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

fn watch_intent(
    home: &Path,
) -> Result<(notify::RecommendedWatcher, tokio::sync::mpsc::Receiver<()>)> {
    let (wake, changes) = tokio::sync::mpsc::channel(1);
    let intent = home.join("service.json");
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // inotify emits Access(Open) for read_intent itself. Never feed reads
        // back into another read, or unrelated scheduler/config writes into the
        // supervisor. Atomic replacement includes the destination in paths.
        if event.is_ok_and(|event| {
            matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            ) && event.paths.iter().any(|path| path == &intent)
        }) {
            let _ = wake.try_send(());
        }
    })?;
    watcher.watch(home, notify::RecursiveMode::NonRecursive)?;
    Ok((watcher, changes))
}

pub async fn kill_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: the child was placed in its own process group above.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = run(
            "taskkill.exe",
            &["/F".into(), "/T".into(), "/PID".into(), pid.to_string()],
        )
        .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

pub async fn run(name: impl AsRef<std::ffi::OsStr>, args: &[String]) -> Result<()> {
    let name = name.as_ref();
    let mut command = Command::new(name);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let result = tokio::time::timeout(Duration::from_secs(15), command.status())
        .await
        .context("system command timed out")??;
    if !result.success() {
        bail!(
            "{} failed; check user service permissions",
            Path::new(name)
                .file_name()
                .unwrap_or(name)
                .to_string_lossy()
        );
    }
    Ok(())
}

pub fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn user_file(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("registration has no parent directory")?;
    fs::create_dir_all(parent)?;
    if let Ok(st) = fs::symlink_metadata(path)
        && (!st.is_file() || st.file_type().is_symlink())
    {
        bail!("refusing nonregular service registration");
    }
    // Do not chmod shared user configuration directories to Noria's mode.
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    staged.write_all(data)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(windows) {
        "user supervisor / Task Scheduler"
    } else {
        "systemd user service / user supervisor"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_are_stable_and_home_scoped() {
        assert_eq!(label(Path::new("/tmp/a")), label(Path::new("/tmp/a")));
        assert_ne!(label(Path::new("/tmp/a")), label(Path::new("/tmp/b")));
        assert_eq!(label(Path::new("/tmp/a")).len(), 29);
    }
    #[test]
    fn intent_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        set_intent(temp.path(), Path::new("/bin/noria"), true).unwrap();
        assert!(read_intent(temp.path()).wanted);
        set_intent(temp.path(), Path::new("/bin/noria"), false).unwrap();
        assert!(!read_intent(temp.path()).wanted);
    }

    #[tokio::test]
    async fn intent_watcher_sleeps_on_reads_and_wakes_on_atomic_replacement() {
        let temp = tempfile::tempdir().unwrap();
        set_intent(temp.path(), Path::new("noria"), true).unwrap();
        let (_watcher, mut changes) = watch_intent(temp.path()).unwrap();
        for _ in 0..10 {
            assert!(read_intent(temp.path()).wanted);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(150), changes.recv())
                .await
                .is_err()
        );
        set_intent(temp.path(), Path::new("noria"), false).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), changes.recv())
                .await
                .unwrap(),
            Some(())
        );
        assert!(!read_intent(temp.path()).wanted);
    }
}
