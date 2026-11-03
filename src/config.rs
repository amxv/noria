//! Private atomic configuration and cross-process advisory locking.
use crate::model::Account;
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:43119";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub accounts: Vec<Account>,
    pub start_at_login: bool,
    pub menubar_at_login: bool,
    pub auto_update: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tool_path: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.into(),
            accounts: vec![],
            start_at_login: false,
            menubar_at_login: false,
            auto_update: true,
            tool_path: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Preferences {
    pub launch_menubar_at_login: bool,
    pub start_at_login: bool,
    pub auto_update: bool,
}

pub fn home() -> Result<PathBuf> {
    let path = match std::env::var_os("NORIA_HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .context("cannot determine user home")?,
        )
        .join(".noria"),
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    // Match filepath.Abs's lexical cleaning without resolving symlinks or
    // requiring an initialized home directory to exist.
    let mut clean = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    Ok(clean)
}

pub fn private_dir(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("refusing symlink directory {}", path.display())
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    set_mode(path, 0o700)?;
    Ok(())
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Write a fully synced private temporary file, then atomically replace target.
pub fn write(path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
    write_with(path, |f| {
        f.write_all(data)?;
        Ok(())
    })
}

/// Stream large metadata files into an atomic replacement without holding a
/// second serialized copy of the complete scheduler state in memory.
pub(crate) fn write_with<F>(path: impl AsRef<Path>, write_data: F) -> Result<()>
where
    F: FnOnce(&mut File) -> Result<()>,
{
    let path = path.as_ref();
    let dir = path.parent().context("target has no parent")?;
    private_dir(dir)?;
    let temp = dir.join(format!(".noria-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&temp)?;
        set_mode(&temp, 0o600)?;
        write_data(&mut f)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temp, path)?;
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub fn init(home: impl AsRef<Path>) -> Result<()> {
    let home = home.as_ref();
    private_dir(home)?;
    let _guard = lock(home, "config")?;
    let path = home.join("config.json");
    match fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => save(
            home,
            &Config {
                tool_path: std::env::var("PATH").unwrap_or_default(),
                ..Config::default()
            },
        )?,
        Err(e) => return Err(e.into()),
        _ => {}
    }
    if let Ok(mut existing) = load(home)
        && existing.tool_path.is_empty()
    {
        existing.tool_path = std::env::var("PATH").unwrap_or_default();
        save(home, &existing)?;
    }
    match fs::metadata(home.join("secret")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut secret = [0_u8; 32];
            getrandom::fill(&mut secret)
                .map_err(|_| anyhow::anyhow!("secure random generator unavailable"))?;
            let encoded: String = secret.iter().map(|b| format!("{b:02x}")).collect();
            write(home.join("secret"), encoded.as_bytes())?;
        }
        Err(e) => return Err(e.into()),
        _ => {}
    }
    Ok(())
}

pub fn load(home: impl AsRef<Path>) -> Result<Config> {
    let home = home.as_ref();
    let bytes =
        fs::read(home.join("config.json")).context("read configuration (run noria init)")?;
    let c: Config = serde_json::from_slice(&bytes).context("invalid configuration")?;
    validate_listen(&c.listen)?;
    let mut ids = HashSet::new();
    for a in &c.accounts {
        validate_id(&a.id)?;
        if !ids.insert(&a.id) {
            bail!("duplicate account {:?}", a.id);
        }
        if a.provider != "codex" && a.provider != "claude" {
            bail!("invalid provider for {}", a.id);
        }
        let expected = home.join("accounts").join(&a.id);
        if Path::new(&a.profile) != expected {
            bail!("account {} profile must be {}", a.id, expected.display());
        }
    }
    Ok(c)
}

pub fn save(home: impl AsRef<Path>, config: &Config) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(config)?;
    data.push(b'\n');
    write(home.as_ref().join("config.json"), &data)
}
pub fn secret(home: impl AsRef<Path>) -> Result<String> {
    let s = fs::read_to_string(home.as_ref().join("secret"))?;
    if s.len() != 64 {
        bail!("invalid local secret; restore a 64-character secret");
    }
    Ok(s)
}
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        bail!("account ID must be 1-64 letters, digits, underscores or hyphens");
    }
    Ok(())
}
pub fn validate_listen(addr: &str) -> Result<()> {
    let addr: SocketAddr = addr
        .parse()
        .context("invalid listen address: expected numeric host and port")?;
    let loopback = match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v| v.is_loopback()),
    };
    if !loopback {
        bail!("listen must use a numeric loopback address");
    }
    Ok(())
}

pub struct FileLock(File);
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
pub fn lock(home: impl AsRef<Path>, name: &str) -> Result<FileLock> {
    lock_timeout(home, name, Duration::from_secs(3))
}
pub fn lock_timeout(home: impl AsRef<Path>, name: &str, timeout: Duration) -> Result<FileLock> {
    let home = home.as_ref();
    private_dir(home)?;
    let p = home.join(format!("{name}.lock"));
    match fs::symlink_metadata(&p) {
        Ok(m) if !m.file_type().is_file() => bail!("invalid lock file {}", p.display()),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&p)?;
    if !file.metadata()?.is_file() {
        bail!("invalid lock file {}", p.display());
    }
    let start = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = file.set_len(0);
                let _ = writeln!(file, "pid={}", std::process::id());
                return Ok(FileLock(file));
            }
            Err(e)
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.raw_os_error() != fs2::lock_contended_error().raw_os_error() =>
            {
                return Err(e.into());
            }
            Err(_) if start.elapsed() >= timeout => bail!("{name} is busy: deadline exceeded"),
            Err(_) => std::thread::sleep(
                Duration::from_millis(30).min(timeout.saturating_sub(start.elapsed())),
            ),
        }
    }
}

pub fn mutate<F>(home: impl AsRef<Path>, f: F) -> Result<()>
where
    F: FnOnce(&mut Config) -> Result<()>,
{
    let home = home.as_ref();
    let _guard = lock(home, "config")?;
    let mut c = load(home)?;
    f(&mut c)?;
    save(home, &c)
}
pub fn change<F>(home: impl AsRef<Path>, f: F) -> Result<()>
where
    F: FnOnce(&mut Config) -> Result<()>,
{
    mutate(home, f)
}
pub fn read_preferences(home: impl AsRef<Path>) -> Result<Preferences> {
    let c = load(home)?;
    Ok(Preferences {
        launch_menubar_at_login: c.menubar_at_login,
        start_at_login: c.start_at_login,
        auto_update: c.auto_update,
    })
}
pub fn save_preferences(home: impl AsRef<Path>, p: Preferences) -> Result<()> {
    mutate(home, |c| {
        c.menubar_at_login = p.launch_menubar_at_login;
        c.start_at_login = p.start_at_login;
        c.auto_update = p.auto_update;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_roundtrip_and_stale_lock() {
        let temp = tempfile::tempdir().unwrap();
        let h = temp.path().join("noria");
        init(&h).unwrap();
        let before = secret(&h).unwrap();
        init(&h).unwrap();
        assert_eq!(secret(&h).unwrap(), before);
        mutate(&h, |c| {
            c.accounts.push(Account {
                id: "work".into(),
                provider: "codex".into(),
                profile: h.join("accounts/work").to_string_lossy().into(),
                enabled: true,
                ..Account::default()
            });
            Ok(())
        })
        .unwrap();
        let c = load(&h).unwrap();
        save(&h, &c).unwrap();
        assert_eq!(c, load(&h).unwrap());
        assert!(c.auto_update);
        fs::write(h.join("stale.lock"), b"pid=999999\n").unwrap();
        let guard = lock(&h, "stale").unwrap();
        let error = lock_timeout(&h, "stale", Duration::from_millis(50))
            .err()
            .expect("a held lock must time out");
        assert_eq!(error.to_string(), "stale is busy: deadline exceeded");
        drop(guard);
        assert!(lock(&h, "stale").is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&h).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(h.join("config.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn rejects_traversal_and_remote_listen() {
        for id in ["", "../user", "-start", "a.b", "user/name"] {
            assert!(validate_id(id).is_err());
        }
        for addr in ["0.0.0.0:43119", "example.com:80", "[::]:43119"] {
            assert!(validate_listen(addr).is_err());
        }
        for addr in ["127.0.0.1:43119", "[::1]:43119"] {
            validate_listen(addr).unwrap();
        }
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink_directories_and_lock_files() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write(link.join("config.json"), b"no").is_err());
        fs::write(target.join("external"), b"unchanged").unwrap();
        std::os::unix::fs::symlink(target.join("external"), target.join("native.lock")).unwrap();
        assert!(lock(&target, "native").is_err());
        assert_eq!(fs::read(target.join("external")).unwrap(), b"unchanged");
    }
}
