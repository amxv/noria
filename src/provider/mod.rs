//! Provider-owned authentication storage and passive native control protocols.
mod claude;
mod codex;
mod credentials;
mod identity;
mod login;
mod process;
#[cfg(test)]
mod tests;

use crate::{
    config,
    model::{Account, Credential, Snapshot},
};
use anyhow::{Context, Result, bail};
pub use claude::parse_claude;
pub use codex::parse_codex;
pub use identity::Identity;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct Manager {
    home: PathBuf,
    cache: Mutex<HashMap<PathBuf, (Snapshot, Instant)>>,
    codex_command: PathBuf,
    claude_command: PathBuf,
    claude_usage_url: String,
    claude_token_url: String,
    usage_client: reqwest::Client,
    #[cfg(test)]
    login_opener: Option<fn(&str) -> Result<()>>,
}
impl Manager {
    pub fn new(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref().to_path_buf();
        let usage_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(12))
            .build()
            .expect("build private OAuth HTTP client");
        Self {
            codex_command: resolve_native(&home, "codex"),
            claude_command: resolve_native(&home, "claude"),
            home,
            cache: Mutex::new(HashMap::new()),
            claude_usage_url: "https://api.anthropic.com/api/oauth/usage".into(),
            claude_token_url: "https://platform.claude.com/v1/oauth/token".into(),
            usage_client,
            #[cfg(test)]
            login_opener: None,
        }
    }
    async fn profile(&self, a: &Account) -> Result<PathBuf> {
        if a.provider != "codex" && a.provider != "claude" {
            bail!("unsupported provider");
        }
        if !Path::new(&a.profile).is_absolute() {
            bail!("account profile must be absolute");
        }
        let root = tokio::fs::canonicalize(&self.home)
            .await
            .map_err(|_| anyhow::anyhow!("Noria home is unavailable"))?;
        let p = tokio::fs::canonicalize(&a.profile)
            .await
            .map_err(|_| anyhow::anyhow!("account profile is unavailable; run account login"))?;
        if p == root || !p.starts_with(&root) {
            bail!("account profile must be isolated below Noria home");
        }
        Ok(p)
    }
    async fn lock_profile(&self, p: &Path) -> Result<config::FileLock> {
        // Never block a Tokio worker or hold a manager-wide lock during I/O.
        // Short attempts also make cancellation release pending lock work promptly.
        loop {
            let p = p.to_path_buf();
            let attempt = tokio::task::spawn_blocking(move || {
                config::lock_timeout(p, "native", Duration::ZERO)
            })
            .await?;
            match attempt {
                Ok(g) => return Ok(g),
                Err(e) if e.to_string().contains("is busy: deadline exceeded") => {
                    tokio::time::sleep(Duration::from_millis(30)).await
                }
                Err(e) => return Err(e),
            }
        }
    }
    pub async fn credentials(&self, a: &Account) -> Result<Credential> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let p = self.profile(a).await?;
            let (c, expired) = self.read_credential(a, &p).await?;
            if !expired {
                return Ok(c);
            }
            let _guard = self.lock_profile(&p).await?;
            let (c, expired) = self.read_credential(a, &p).await?;
            if !expired {
                return Ok(c);
            }
            if a.provider == "codex" {
                self.codex_control(a, &p, true).await.map(|_| ())
            } else {
                self.refresh_claude_oauth(a, &p).await
            }
            .context("native credential refresh failed; run account login")?;
            let (c, expired) = self.read_credential(a, &p).await?;
            if expired {
                bail!("native account credentials expired; run account login");
            }
            Ok(c)
        })
        .await
        .map_err(|_| anyhow::anyhow!("native credentials deadline exceeded"))?
    }
    pub async fn refresh(&self, a: &Account) -> Result<Snapshot> {
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(30), async {
            let p = self.profile(a).await?;
            let _guard = self.lock_profile(&p).await?;
            {
                let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((cached, completed)) = cache.get(&p)
                    && *completed > started
                {
                    let mut snapshot = cached.clone();
                    snapshot.account_id = a.id.clone();
                    return Ok(snapshot);
                }
            }
            let snapshot = if a.provider == "codex" {
                self.codex_control(a, &p, false).await?
            } else {
                self.claude_quota(a, &p).await?
            };
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(p, (snapshot.clone(), Instant::now()));
            Ok(snapshot)
        })
        .await
        .map_err(|_| anyhow::anyhow!("native quota deadline exceeded"))?
    }
    pub fn doctor(&self) -> Vec<String> {
        [&self.codex_command, &self.claude_command]
            .into_iter()
            .filter(|p| find_executable(p).is_none())
            .map(|p| format!("{} executable unavailable", p.display()))
            .collect()
    }
}

fn find_executable(path: &Path) -> Option<PathBuf> {
    let valid = |p: &Path| {
        std::fs::metadata(p).ok().is_some_and(|m| {
            if !m.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    };
    if path.components().count() > 1 {
        return valid(path).then(|| path.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join(path))
        .find_map(|p| {
            if valid(&p) {
                return Some(p);
            }
            #[cfg(windows)]
            {
                let exe = p.with_extension("exe");
                if valid(&exe) {
                    return Some(exe);
                }
            }
            None
        })
}
fn resolve_native(home: &Path, name: &str) -> PathBuf {
    if let Some(p) = find_executable(Path::new(name)) {
        return p;
    }
    if let Ok(config) = config::load(home) {
        for dir in std::env::split_paths(&config.tool_path) {
            if !dir.is_absolute() {
                continue;
            }
            let path = dir.join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.into()
            });
            if path.is_file() {
                return path;
            }
        }
    }
    PathBuf::from(name)
}
