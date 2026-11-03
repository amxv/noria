use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{ffi::OsString, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub(super) fn clean_env(profile: &Path, provider: &str) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            let u = key.to_string_lossy().to_ascii_uppercase();
            ![
                "ANTHROPIC_",
                "OPENAI_",
                "CODEX_",
                "CLAUDE_",
                "NORIA_",
                "NODE_",
            ]
            .iter()
            .any(|prefix| u.starts_with(prefix))
                && !["CLAUDECODE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"].contains(&u.as_str())
        })
        .collect();
    env.push((
        if provider == "codex" {
            "CODEX_HOME"
        } else {
            "CLAUDE_CONFIG_DIR"
        }
        .into(),
        profile.as_os_str().into(),
    ));
    env
}
pub(super) fn command(path: &Path, profile: &Path, provider: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(path);
    cmd.args(args)
        .current_dir(profile)
        .env_clear()
        .envs(clean_env(profile, provider))
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure(&mut cmd);
    cmd
}
pub(super) fn configure(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x08000000);
    }
}

/// Killing the process group on Drop makes every cancellation path bounded.
pub(super) struct NativeProcess {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    max_line: usize,
}
impl NativeProcess {
    pub fn spawn(mut cmd: Command, max_line: usize) -> Result<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = cmd
            .spawn()
            .context("cannot start native control executable")?;
        let input = child
            .stdin
            .take()
            .context("cannot open native control input")?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .context("cannot open native control output")?,
        );
        Ok(Self {
            child,
            input,
            output,
            max_line,
        })
    }
    pub async fn write(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.input
            .write_all(&bytes)
            .await
            .context("native control write failed")?;
        self.input.flush().await?;
        Ok(())
    }
    pub async fn read(&mut self) -> Result<Value> {
        let mut line = Vec::with_capacity(4096);
        loop {
            let available = self
                .output
                .fill_buf()
                .await
                .context("native control read failed")?;
            if available.is_empty() {
                bail!("native control closed or deadline exceeded");
            }
            let n = available
                .iter()
                .position(|b| *b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(available.len());
            if line.len() + n > self.max_line {
                bail!("native control response exceeds size limit");
            }
            let ended = available[n - 1] == b'\n';
            line.extend_from_slice(&available[..n]);
            self.output.consume(n);
            if ended {
                return serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("invalid native control response"));
            }
        }
    }
}
impl Drop for NativeProcess {
    fn drop(&mut self) {
        kill_tree(&mut self.child);
    }
}
fn kill_tree(child: &mut Child) {
    if let Some(id) = child.id() {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &id.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _ = child.start_kill();
}

pub(super) async fn bounded_output(mut cmd: Command, max: usize) -> Result<Vec<u8>> {
    cmd.stdout(Stdio::piped()).stdin(Stdio::null());
    let mut child = cmd.spawn().context("cannot start native executable")?;
    // Reuse the same group-on-drop protection for noninteractive identity/login.
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            kill_tree(&mut self.0);
        }
    }
    let mut stdout = child.stdout.take().context("native output unavailable")?;
    let mut guard = Guard(child);
    let mut data = Vec::new();
    (&mut stdout)
        .take(max as u64 + 1)
        .read_to_end(&mut data)
        .await?;
    if data.len() > max {
        bail!("native response exceeds size limit");
    }
    if !guard.0.wait().await?.success() {
        bail!("native executable failed");
    }
    Ok(data)
}

pub(super) async fn run_silent(mut cmd: Command) -> Result<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    let child = cmd.spawn()?;
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            kill_tree(&mut self.0);
        }
    }
    let mut guard = Guard(child);
    if !guard.0.wait().await?.success() {
        bail!("native executable failed");
    }
    Ok(())
}
