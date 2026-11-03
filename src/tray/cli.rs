use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Cli {
    pub home: PathBuf,
    pub exe: PathBuf,
}

impl Cli {
    pub fn new(home: Option<PathBuf>) -> Result<Self> {
        let home = home
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| {
                std::env::var_os("NORIA_HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or(user_home()?.join(".noria"));
        let home = std::path::absolute(home)?;
        let exe = std::env::current_exe()?.with_file_name(if cfg!(windows) {
            "noria.exe"
        } else {
            "noria"
        });
        ensure!(
            exe.is_file(),
            "Noria CLI is not installed beside the tray executable"
        );
        Ok(Self { home, exe })
    }

    pub async fn run(
        &self,
        args: &[String],
        seconds: u64,
        cancel: CancellationToken,
    ) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.exe);
        command
            .args(args)
            .arg("--home")
            .arg(&self.home)
            .current_dir(&self.home)
            .env("PATH", gui_path()?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = command.spawn().context("start Noria CLI")?;
        let pid = child.id();
        let stdout = child.stdout.take().context("capture CLI output")?;
        let stderr = child.stderr.take().context("capture CLI errors")?;
        let mut output = tokio::spawn(read_bounded(stdout, 4 << 20));
        let mut errors = tokio::spawn(read_bounded(stderr, 32 << 10));
        let status = tokio::select! {
            result = child.wait() => Some(result?),
            _ = tokio::time::sleep(Duration::from_secs(seconds)) => None,
            _ = cancel.cancelled() => None,
        };
        if status.is_none() {
            // Stop the whole isolated command group so native sign-in children cannot
            // survive cancellation. The CLI also owns provider-specific cleanup.
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
                // Let the CLI's signal handler clean up provider subprocesses
                // (which can have their own process groups) before forcing exit.
                if tokio::time::timeout(Duration::from_secs(2), child.wait())
                    .await
                    .is_err()
                {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
            }
            #[cfg(windows)]
            if let Some(pid) = pid {
                let mut terminate = Command::new("taskkill.exe");
                terminate
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .creation_flags(0x0800_0000)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                let _ = tokio::time::timeout(Duration::from_secs(2), terminate.status()).await;
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            output.abort();
            errors.abort();
            if cancel.is_cancelled() {
                bail!("Noria command canceled");
            }
            bail!("Noria command timed out");
        }
        // Bound reader lifetime too: a detached descendant must not hold a pipe
        // open forever after its parent CLI has already exited.
        let captured = tokio::time::timeout(Duration::from_secs(2), async {
            Ok::<_, anyhow::Error>(((&mut output).await??, (&mut errors).await??))
        })
        .await;
        let (stdout, stderr) = match captured {
            Ok(result) => result?,
            Err(_) => {
                output.abort();
                errors.abort();
                bail!("Noria command output did not close");
            }
        };
        if !status.unwrap().success() {
            let detail = String::from_utf8_lossy(&stderr)
                .trim()
                .chars()
                .take(500)
                .collect::<String>();
            bail!(
                "{}",
                if detail.is_empty() {
                    "Noria command failed"
                } else {
                    &detail
                }
            );
        }
        Ok(stdout)
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    maximum: usize,
) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let length = reader.read(&mut buffer).await?;
        if length == 0 {
            return Ok(data);
        }
        let keep = length.min(maximum.saturating_sub(data.len()));
        data.extend_from_slice(&buffer[..keep]);
    }
}

pub fn user_home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .context("home directory unavailable")
}

fn gui_path() -> Result<OsString> {
    let home = user_home()?;
    let mut entries = vec![
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".cargo/bin"),
    ];
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            entries.push(PathBuf::from(appdata).join("npm"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            entries.push(PathBuf::from(local).join("Programs/nodejs"));
        }
    } else {
        entries.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    }
    if let Some(existing) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&existing));
    }
    Ok(std::env::join_paths(entries)?)
}
