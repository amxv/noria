use super::*;
use crate::distribution;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use tokio::process::Command;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct UpdateStatus {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub updated: bool,
    pub deferred: bool,
    pub checked_at: DateTime<Utc>,
    pub message: String,
}
fn record(home: &Path, status: &UpdateStatus) -> Result<()> {
    config::write(home.join("upgrade.json"), &serde_json::to_vec(status)?)
}
fn previous_status(home: &Path) -> Option<UpdateStatus> {
    fs::read(home.join("upgrade.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}
pub async fn command(home: &Path, mut args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!(
            "Usage: noria upgrade [--check] [--json]\nChecksum-verified CLI and companion desktop updates; active requests defer installation."
        );
        return Ok(());
    }
    let check = flag(&mut args, "--check");
    let automatic = flag(&mut args, "--automatic");
    flag(&mut args, "--json");
    no_args(&args, "noria upgrade [--check] [--automatic] [--json]")?;
    config::init(home)?;
    let _upgrade = config::lock(home, "upgrade")?;
    if automatic {
        if !config::read_preferences(home)?.auto_update {
            return Ok(());
        }
        if previous_status(home).is_some_and(|s| {
            !s.available && Utc::now().signed_duration_since(s.checked_at).num_seconds() < 6 * 3600
        }) {
            return Ok(());
        }
    }
    let release = tokio::time::timeout(Duration::from_secs(120), distribution::latest())
        .await
        .context("release lookup timed out")??;
    let mut status = UpdateStatus {
        current: crate::VERSION.into(),
        latest: release.tag.trim_start_matches('v').into(),
        available: distribution::newer(&release.tag, crate::VERSION),
        checked_at: Utc::now(),
        ..Default::default()
    };
    if !status.available || check {
        status.message = if status.available {
            "Update available"
        } else {
            "Up to date"
        }
        .into();
        record(home, &status)?;
        return print_json(&status);
    }
    if automatic
        && previous_status(home).is_some_and(|s| {
            s.updated && Utc::now().signed_duration_since(s.checked_at).num_seconds() < 3600
        })
    {
        return Ok(());
    }
    let temp = tempfile::Builder::new()
        .prefix(".release-")
        .tempdir_in(home)?;
    let archive = temp.path().join("release.tar.gz");
    distribution::download(&release, &distribution::archive_name(), &archive).await?;
    let unpacked = temp.path().join("unpacked");
    distribution::extract(&archive, &unpacked)?;
    let source = unpacked.join(if cfg!(windows) { "noria.exe" } else { "noria" });
    let exe = service::executable()?;
    let tray_source = if cfg!(target_os = "macos") {
        None
    } else {
        Some(unpacked.join(distribution::tray_name()))
    };
    let app_source = if cfg!(target_os = "macos") {
        let path = unpacked.join("Noria.app");
        verify_app(&path).await?;
        Some(path)
    } else {
        None
    };
    // Slow network and extraction work is complete before serializing lifecycle.
    let _lifecycle = config::lock(home, "lifecycle")?;
    let was_running = running(home).await;
    if was_running {
        let drain = control(home, reqwest::Method::POST, "/_noria/drain", None)
            .await
            .context("running service cannot prepare a safe update; stop Noria first")?;
        let busy = drain["busy"]
            .as_bool()
            .context("invalid service drain response")?;
        if busy {
            status.deferred = true;
            status.message = "Update ready; waiting for active requests to finish".into();
            record(home, &status)?;
            return print_json(&status);
        }
    }
    let outcome = install(
        home,
        &exe,
        &source,
        tray_source.as_deref(),
        app_source.as_deref(),
        was_running,
        &status.latest,
    )
    .await;
    if was_running {
        let _ = control_timeout(
            home,
            reqwest::Method::POST,
            "/_noria/resume",
            None,
            Duration::from_secs(1),
        )
        .await;
    }
    let deferred = outcome?;
    status.deferred = deferred;
    status.updated = !deferred;
    status.message = if deferred {
        "Verified upgrade staged. Noria will finish replacing the CLI and tray after this command exits.".into()
    } else {
        format!("Updated to {}", status.latest)
    };
    record(home, &status)?;
    if !deferred
        && tray_source.is_some()
        && cfg!(target_os = "linux")
        && std::env::var("NORIA_NO_LAUNCH").as_deref() != Ok("1")
        && desktop::tray_environment().is_ok()
    {
        // Invoke the newly installed CLI: current_exe can point at the rollback
        // inode after a Unix binary swap, so don't resolve it again here.
        let mut command = Command::new(&exe);
        command
            .args(["tray", "open", "--home"])
            .arg(home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        service::detached(&mut command);
        let _ = command.spawn();
    }
    print_json(&status)
}
async fn probe(source: &Path, version: &str, tray: bool) -> Result<()> {
    distribution::executable_mode(source)?;
    let mut command = Command::new(source);
    command
        .arg(if tray { "--check" } else { "--version" })
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("downloaded executable verification timed out")??;
    let expected = if tray {
        format!("noria-tray {version} ready")
    } else {
        format!("noria {version}")
    };
    ensure!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == expected,
        "downloaded executable did not pass version verification"
    );
    Ok(())
}
async fn install(
    home: &Path,
    exe: &Path,
    source: &Path,
    tray_source: Option<&Path>,
    app_source: Option<&Path>,
    was_running: bool,
    version: &str,
) -> Result<bool> {
    probe(source, version, false).await?;
    if let Some(tray) = tray_source {
        probe(tray, version, true).await?;
    }
    if was_running {
        stop_runtime(home, exe).await?;
    }
    #[cfg(windows)]
    {
        if let Err(error) =
            super::windows_update::schedule(home, source, exe, tray_source, was_running, version)
                .await
        {
            if was_running {
                let _ = start_at(home, exe).await;
            }
            return Err(error);
        }
        let _ = app_source;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let mut tray_swap = None;
        let mut cli_swap = None;
        let mut app_changed = false;
        let app_target = desktop::menu_path(exe);
        let result: Result<()> = async {
            if let Some(tray) = tray_source {
                tray_swap = Some(distribution::swap(tray, &desktop::tray_binary(exe))?);
            }
            cli_swap = Some(distribution::swap(source, exe)?);
            if let Some(app) = app_source
                && app_target.exists()
            {
                replace_app(app, &app_target).await?;
                app_changed = true;
            }
            if was_running {
                start_at(home, exe)
                    .await
                    .context("updated service did not become healthy")?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            // Stop any partially started new process before restoring its pair.
            if was_running {
                let _ = stop_runtime(home, exe).await;
            }
            drop(cli_swap);
            drop(tray_swap);
            if app_changed {
                let _ = fs::remove_dir_all(&app_target);
                let _ = fs::rename(app_backup(&app_target), &app_target);
            }
            if was_running {
                let _ = start_at(home, exe).await;
            }
            return Err(error.context("installation failed; restored the previous release"));
        }
        if let Some(swap) = cli_swap {
            swap.finish();
        }
        if let Some(swap) = tray_swap {
            swap.finish();
        }
        if app_changed {
            let _ = fs::remove_dir_all(app_backup(&app_target));
        }
        Ok(false)
    }
}
pub(super) async fn verify_app(path: &Path) -> Result<()> {
    service::run(
        "/usr/bin/codesign",
        &[
            "--verify".into(),
            "--deep".into(),
            "--strict".into(),
            path.to_string_lossy().into_owned(),
        ],
    )
    .await
    .context("menu bar app signature verification failed")
}
fn app_backup(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.previous", path.display()))
}
pub(super) async fn replace_app(source: &Path, target: &Path) -> Result<()> {
    let stage = PathBuf::from(format!("{}.staged", target.display()));
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    if let Err(error) = copy_tree(source, &stage) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    if let Err(error) = verify_app(&stage).await {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let backup = app_backup(target);
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    let had = target.exists();
    if had {
        ensure!(
            fs::symlink_metadata(target)?.is_dir(),
            "application path is not a directory"
        );
        fs::rename(target, &backup)?;
    }
    if let Err(error) = fs::rename(&stage, target) {
        if had {
            let _ = fs::rename(&backup, target);
        }
        return Err(error.into());
    }
    Ok(())
}
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let info = fs::symlink_metadata(source)?;
    ensure!(
        !info.file_type().is_symlink(),
        "application bundle cannot contain symlinks"
    );
    if info.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        ensure!(info.is_file(), "application bundle contains a special file");
        let mut from = fs::File::open(source)?;
        let mut to = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(target)?;
        std::io::copy(&mut from, &mut to)?;
        fs::set_permissions(target, info.permissions())?;
        to.sync_all()?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn app_copy_rejects_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("source");
        fs::create_dir(&source).unwrap();
        std::os::unix::fs::symlink("/tmp", source.join("link")).unwrap();
        assert!(copy_tree(&source, &t.path().join("target")).is_err());
    }
}
