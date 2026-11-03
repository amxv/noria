//! Bounded, checksum-verified release downloads and transactional installation.
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
    #[serde(default)]
    pub size: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Release {
    #[serde(rename = "tag_name")]
    pub tag: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

fn version(value: &str) -> Option<[u64; 3]> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}
pub fn newer(target: &str, current: &str) -> bool {
    version(target).is_some_and(|a| version(current).is_none_or(|b| a > b))
}
pub fn platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "windows"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    (os, arch)
}
pub fn binary_name() -> String {
    let (os, arch) = platform();
    format!(
        "noria_{os}_{arch}{}",
        if cfg!(windows) { ".exe" } else { "" }
    )
}
pub fn archive_name() -> String {
    let (os, arch) = platform();
    format!("noria_{os}_{arch}.tar.gz")
}
pub fn tray_name() -> &'static str {
    if cfg!(windows) {
        "noria-tray.exe"
    } else {
        "noria-tray"
    }
}
pub fn tagged(value: &str) -> Result<Release> {
    ensure!(version(value).is_some(), "invalid release version");
    let tag = format!("v{}", value.trim_start_matches('v'));
    let assets = ["SHA256SUMS".to_owned(), binary_name(), archive_name()]
        .into_iter()
        .map(|name| Asset {
            url: format!("https://github.com/amxv/noria/releases/download/{tag}/{name}"),
            name,
            size: 0,
        })
        .collect();
    Ok(Release {
        tag,
        draft: false,
        prerelease: false,
        assets,
    })
}
fn latest_tag(location: &str) -> Result<String> {
    let url = url::Url::parse("https://github.com/amxv/noria/releases/latest")?.join(location)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "untrusted latest-release redirect"
    );
    let tag = url
        .path()
        .strip_prefix("/amxv/noria/releases/tag/")
        .context("untrusted latest-release redirect")?;
    ensure!(
        tag.starts_with('v') && version(tag).is_some(),
        "invalid latest release tag"
    );
    Ok(tag.to_owned())
}
pub async fn latest() -> Result<Release> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Noria-updater")
        .build()?;
    if let Ok(response) = client
        .head("https://github.com/amxv/noria/releases/latest")
        .send()
        .await
        && matches!(response.status().as_u16(), 301 | 302 | 307 | 308)
        && let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|h| h.to_str().ok())
        && let Ok(tag) = latest_tag(location)
    {
        return tagged(&tag);
    }
    let data = get_bounded(
        "https://api.github.com/repos/amxv/noria/releases/latest",
        2 << 20,
    )
    .await?;
    let release: Release = serde_json::from_slice(&data)?;
    ensure!(
        !release.draft && !release.prerelease && version(&release.tag).is_some(),
        "release is not a stable published version"
    );
    Ok(release)
}
fn trusted(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "api.github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
            )
        )
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .user_agent("Noria-updater")
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 || !trusted(attempt.url()) {
                attempt.error("unsafe release redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}
async fn get(address: &str) -> Result<reqwest::Response> {
    ensure!(
        trusted(&url::Url::parse(address)?),
        "untrusted release endpoint"
    );
    let response = client()?
        .get(address)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "release server returned HTTP {}",
        response.status().as_u16()
    );
    Ok(response)
}
async fn get_bounded(address: &str, limit: usize) -> Result<Vec<u8>> {
    let response = get(address).await?;
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        "release asset exceeds size limit"
    );
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            data.len() + chunk.len() <= limit,
            "release asset exceeds size limit"
        );
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}
fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str> {
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .with_context(|| format!("release {} is missing {name}", release.tag))?;
    let url = url::Url::parse(&asset.url)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == format!("/amxv/noria/releases/download/{}/{name}", release.tag)
            && url.query().is_none()
            && url.fragment().is_none(),
        "untrusted release download origin"
    );
    Ok(&asset.url)
}
fn checksum(sums: &str, name: &str) -> Result<String> {
    let mut found = None;
    for line in sums.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() == 2 && parts[1].trim_start_matches('*') == name {
            ensure!(found.is_none(), "duplicate checksum entry");
            found = Some(parts[0]);
        }
    }
    let sum = found.context("release checksum missing")?;
    ensure!(
        sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid checksum"
    );
    Ok(sum.to_ascii_lowercase())
}
pub async fn download(release: &Release, name: &str, dest: &Path) -> Result<()> {
    let sums = get_bounded(asset_url(release, "SHA256SUMS")?, 1 << 20).await?;
    let expected = checksum(std::str::from_utf8(&sums)?, name)?;
    let response = get(asset_url(release, name)?).await?;
    ensure!(
        response.content_length().is_none_or(|n| n <= 128 << 20),
        "release asset exceeds size limit"
    );
    // Hash while streaming to a private sibling file: archives never occupy a
    // second 128 MiB memory buffer, and mismatches cannot leave executable data.
    let staged =
        tempfile::NamedTempFile::new_in(dest.parent().context("download path has no parent")?)?;
    let mut file = tokio::fs::File::from_std(staged.reopen()?);
    let mut stream = response.bytes_stream();
    let mut digest = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size += chunk.len() as u64;
        ensure!(size <= 128 << 20, "release asset exceeds size limit");
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    ensure!(
        format!("{:x}", digest.finalize()) == expected,
        "release checksum mismatch"
    );
    staged.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

fn safe_archive_path(raw: &str) -> Result<PathBuf> {
    ensure!(
        !raw.starts_with('/') && !raw.contains(['\\', ':', '\0']),
        "unsafe release archive path"
    );
    let mut clean = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(p) => clean.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(clean.pop(), "archive escapes target");
            }
            _ => bail!("unsafe release archive path"),
        }
    }
    Ok(clean)
}
fn safe_directories(dest: &Path, relative: &Path) -> Result<()> {
    let mut path = dest.to_path_buf();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            path.push(component.as_os_str());
        }
        match fs::symlink_metadata(&path) {
            Ok(st) => ensure!(
                st.is_dir() && !st.file_type().is_symlink(),
                "archive path contains a link or non-directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut size = 0u64;
    for (count, entry) in archive.entries()?.enumerate() {
        ensure!(count < 2048, "too many archive entries");
        let mut entry = entry?;
        let bytes = entry.path_bytes();
        let raw = std::str::from_utf8(&bytes)?;
        let relative = safe_archive_path(raw)?;
        let len = entry.header().size()?;
        ensure!(len <= 128 << 20, "archive file too large");
        size += len;
        ensure!(size <= 256 << 20, "archive exceeds extracted size limit");
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            safe_directories(dest, &relative)?;
            continue;
        }
        ensure!(
            kind.is_file(),
            "release archive contains a link or unsupported entry"
        );
        safe_directories(dest, relative.parent().unwrap_or(Path::new("")))?;
        let mut opts = fs::OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(if entry.header().mode()? & 0o111 != 0 {
                0o755
            } else {
                0o644
            });
        }
        let mut out = opts.open(dest.join(relative))?;
        let written = std::io::copy(&mut entry.by_ref().take(len), &mut out)?;
        ensure!(written == len, "truncated archive file");
        out.sync_all()?;
    }
    Ok(())
}
pub fn executable_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    let _ = path;
    Ok(())
}

pub struct Swap {
    target: PathBuf,
    backup: PathBuf,
    had_previous: bool,
    active: bool,
}
impl Swap {
    pub fn rollback(mut self) -> Result<()> {
        self.rollback_inner()
    }
    fn rollback_inner(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        if self.target.exists() {
            fs::remove_file(&self.target)?;
        }
        if self.had_previous {
            fs::rename(&self.backup, &self.target)?;
        }
        self.active = false;
        Ok(())
    }
    pub fn finish(mut self) {
        if self.had_previous {
            let _ = fs::remove_file(&self.backup);
        }
        self.active = false;
    }
}
impl Drop for Swap {
    fn drop(&mut self) {
        let _ = self.rollback_inner();
    }
}
pub fn swap(source: &Path, target: &Path) -> Result<Swap> {
    let parent = target.parent().context("installed path has no parent")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut fs::File::open(source)?, &mut staged)?;
    executable_mode(staged.path())?;
    staged.as_file().sync_all()?;
    let backup = staged.path().with_extension("previous");
    let had_previous = match fs::symlink_metadata(target) {
        Ok(st) => {
            ensure!(
                st.is_file() && !st.file_type().is_symlink(),
                "installed binary is not a regular file"
            );
            fs::rename(target, &backup)?;
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if let Err(e) = staged.persist(target) {
        if had_previous {
            let _ = fs::rename(&backup, target);
        }
        return Err(e.error.into());
    }
    Ok(Swap {
        target: target.to_path_buf(),
        backup,
        had_previous,
        active: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_versions_and_trusted_redirects() {
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(!newer("v0.4.2", "0.4.2"));
        assert!(!newer("1.2.3-beta", "1.0.0"));
        assert_eq!(
            latest_tag("/amxv/noria/releases/tag/v0.4.2").unwrap(),
            "v0.4.2"
        );
        for url in [
            "https://evil.test/amxv/noria/releases/tag/v1.2.3",
            "https://github.com:444/amxv/noria/releases/tag/v1.2.3",
            "/amxv/noria/releases/tag/v1.2.3?x=1",
        ] {
            assert!(latest_tag(url).is_err());
        }
    }
    #[test]
    fn rejects_duplicate_and_invalid_checksums() {
        let sum = "a".repeat(64);
        assert_eq!(checksum(&format!("{sum}  noria"), "noria").unwrap(), sum);
        assert!(checksum(&format!("{sum}  noria\n{sum}  noria"), "noria").is_err());
        assert!(checksum("bad  noria", "noria").is_err());
    }
    #[test]
    fn archive_paths_are_cross_platform() {
        for raw in [
            "../escape",
            "/absolute",
            "C:/drive",
            "a\\b",
            "a/../../b",
            "a:stream",
        ] {
            assert!(safe_archive_path(raw).is_err(), "{raw}");
        }
        assert_eq!(
            safe_archive_path("./Noria.app/Contents/MacOS/Noria").unwrap(),
            Path::new("Noria.app/Contents/MacOS/Noria")
        );
    }
    #[test]
    fn pair_swap_rolls_back_on_drop_and_commits_explicitly() {
        let t = tempfile::tempdir().unwrap();
        let old = t.path().join("noria");
        let new = t.path().join("new");
        fs::write(&old, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        {
            let _transaction = swap(&new, &old).unwrap();
            assert_eq!(fs::read(&old).unwrap(), b"new");
        }
        assert_eq!(fs::read(&old).unwrap(), b"old");
        swap(&new, &old).unwrap().finish();
        assert_eq!(fs::read(&old).unwrap(), b"new");
    }
    #[test]
    fn rejects_symlink_archive_parent() {
        #[cfg(unix)]
        {
            let t = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), t.path().join("escape")).unwrap();
            assert!(safe_directories(t.path(), Path::new("escape/child")).is_err());
        }
    }
}
