//! Verified sibling helper replaces Windows executable images after their owners exit.
use super::*;
use crate::distribution;
use std::{collections::BTreeMap, ffi::OsString, fs, process::Stdio};
use tokio::process::Command;

fn scrub_environment() -> BTreeMap<OsString, OsString> {
    std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy().to_ascii_uppercase();
            key == "NORIA_NO_LAUNCH"
                || !["NORIA_", "OPENAI_", "ANTHROPIC_", "CLAUDE_", "CODEX_"]
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
        })
        .collect()
}
fn stage(dir: &Path, prefix: &str, source: &Path) -> Result<PathBuf> {
    let mut staged = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".exe")
        .tempfile_in(dir)?;
    std::io::copy(&mut fs::File::open(source)?, &mut staged)?;
    staged.as_file().sync_all()?;
    Ok(staged.keep()?.1)
}
pub(super) async fn schedule(
    home: &Path,
    source: &Path,
    target: &Path,
    tray_source: Option<&Path>,
    was_running: bool,
    version: &str,
) -> Result<()> {
    ensure!(
        target
            .file_name()
            .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("noria.exe")),
        "native Windows upgrades require an installer-managed noria.exe"
    );
    let dir = target
        .parent()
        .context("installed executable has no parent")?;
    let pending = stage(dir, ".noria-upgrade-", source)?;
    let mut tray_pending = None;
    let result: Result<()> = async {
        if let Some(source) = tray_source {
            tray_pending = Some(stage(dir, ".noria-tray-upgrade-", source)?);
        }
        let log = service::log_file(home, "upgrade.log")?;
        let mut command = Command::new(&pending);
        command
            .arg("__apply-windows-upgrade")
            .arg(target)
            .arg(was_running.to_string())
            .arg(version)
            .arg(tray_pending.as_deref().unwrap_or(Path::new("")))
            .arg("--home")
            .arg(home)
            .current_dir(home)
            .env_clear()
            .envs(scrub_environment())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        service::detached(&mut command);
        drop(
            command
                .spawn()
                .context("could not start verified upgrade helper")?,
        );
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(pending);
        if let Some(path) = tray_pending {
            let _ = fs::remove_file(path);
        }
    }
    result
}
pub async fn apply(home: &Path, args: Vec<String>) -> Result<()> {
    ensure!(
        args.len() == 3 || args.len() == 4,
        "internal Windows upgrade arguments are invalid"
    );
    let target = std::path::absolute(&args[0])?;
    let exe = service::executable()?;
    ensure!(
        exe.file_name().is_some_and(|s| s
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with(".noria-upgrade-"))
            && target
                .file_name()
                .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("noria.exe"))
            && same_parent(&target, &exe)?,
        "Windows upgrade helper must be beside the native Noria executable"
    );
    let was_running = args[1]
        .parse::<bool>()
        .context("invalid Windows runtime flag")?;
    ensure!(
        args[2] == crate::VERSION,
        "Windows upgrade helper version is invalid"
    );
    let _upgrade = config::lock_timeout(home, "upgrade", Duration::from_secs(45))?;
    let _lifecycle = config::lock_timeout(home, "lifecycle", Duration::from_secs(45))?;
    let tray_stage = args
        .get(3)
        .filter(|v| !v.is_empty())
        .map(std::path::absolute)
        .transpose()?;
    if let Some(path) = &tray_stage {
        ensure!(
            same_parent(path, &target)?
                && path.file_name().is_some_and(|s| s
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .starts_with(".noria-tray-upgrade-"))
                && path
                    .extension()
                    .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("exe")),
            "Windows tray upgrade file must be beside Noria"
        );
    }
    let mut tray_was_running = false;
    if tray_stage.is_some() {
        match config::lock_timeout(home, "tray", Duration::from_millis(150)) {
            Ok(guard) => drop(guard),
            Err(error) if error.to_string().contains("busy") => {
                tray_was_running = true;
                desktop::request_tray_quit(home)?;
                drop(
                    config::lock_timeout(home, "tray", Duration::from_secs(20))
                        .context("existing tray did not exit for update")?,
                );
            }
            Err(error) => return Err(error),
        }
    }
    let mut tray_swap = None;
    let mut cli_swap = None;
    let result: Result<()> = async {
        if let Some(path) = &tray_stage {
            tray_swap = Some(
                retry_swap(
                    path,
                    &desktop::tray_binary(&target),
                    Duration::from_secs(20),
                )
                .await?,
            );
        }
        cli_swap = Some(retry_swap(&exe, &target, Duration::from_secs(30)).await?);
        if was_running {
            start_at(home, &target).await?;
        }
        Ok(())
    }
    .await;
    if let Some(path) = &tray_stage {
        let _ = fs::remove_file(path);
    }
    if let Err(error) = result {
        if was_running {
            let _ = stop_runtime(home, &target).await;
        }
        drop(cli_swap);
        drop(tray_swap);
        if was_running {
            let _ = start_at(home, &target).await;
        }
        if tray_was_running {
            reopen_tray(&target, home);
        }
        return Err(error.context("Windows upgrade failed; restored the previous release"));
    }
    if let Some(swap) = cli_swap {
        swap.finish();
    }
    if let Some(swap) = tray_swap {
        swap.finish();
    }
    if std::env::var("NORIA_NO_LAUNCH").as_deref() != Ok("1") {
        reopen_tray(&target, home);
    }
    let status = update::UpdateStatus {
        current: crate::VERSION.into(),
        latest: crate::VERSION.into(),
        updated: true,
        checked_at: chrono::Utc::now(),
        message: format!("Upgraded to {}", crate::VERSION),
        ..Default::default()
    };
    config::write(home.join("upgrade.json"), &serde_json::to_vec(&status)?)?;
    println!("{}", status.message);
    Ok(())
}
async fn retry_swap(source: &Path, target: &Path, timeout: Duration) -> Result<distribution::Swap> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match distribution::swap(source, target) {
            Ok(swap) => return Ok(swap),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}
fn reopen_tray(exe: &Path, home: &Path) {
    let mut command = Command::new(exe);
    command
        .args(["tray", "open", "--home"])
        .arg(home)
        .current_dir(home)
        .env_clear()
        .envs(scrub_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    service::detached(&mut command);
    let _ = command.spawn();
}

fn same_parent(a: &Path, b: &Path) -> Result<bool> {
    // Windows current_exe/canonicalize may return a verbatim \\?\ path while
    // the installer supplies an ordinary drive path. Compare resolved parents
    // so both spellings refer to the same installed directory.
    let a = a
        .parent()
        .context("upgrade path has no parent")?
        .canonicalize()?;
    let b = b
        .parent()
        .context("upgrade path has no parent")?
        .canonicalize()?;
    Ok(a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_and_verbatim_sibling_paths_match() {
        let temp = tempfile::tempdir().unwrap();
        let ordinary = temp.path().join("noria.exe");
        let canonical = temp
            .path()
            .canonicalize()
            .unwrap()
            .join(".noria-upgrade-fixture.exe");
        assert!(same_parent(&ordinary, &canonical).unwrap());
        let other = tempfile::tempdir().unwrap();
        assert!(!same_parent(&ordinary, &other.path().join("noria.exe")).unwrap());
    }
}
