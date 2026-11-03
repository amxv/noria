//! Reversible, explicitly enabled native client routing. Restoration metadata
//! remains compatible with the Go implementation's integrations.json.
mod codex;
#[cfg(test)]
mod tests;

use crate::config;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Field {
    pub path: Vec<String>,
    pub before: Value,
    pub existed: bool,
    pub installed: Value,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Entry {
    pub provider: String,
    pub path: String,
    pub active: bool,
    pub backup: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root_line: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root_block: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub definition: String,
    pub created: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Store {
    #[serde(default)]
    pub entries: Vec<Entry>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Target {
    pub provider: String,
    pub path: String,
    pub exists: bool,
    pub active: bool,
    pub registered: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct View {
    pub provider: String,
    pub path: String,
    pub active: bool,
    pub healthy: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

pub fn load(home: &Path) -> Result<Store> {
    match fs::read(home.join("integrations.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(error) => Err(error.into()),
    }
}
fn save(home: &Path, store: &Store) -> Result<()> {
    config::write(
        home.join("integrations.json"),
        &serde_json::to_vec_pretty(store)?,
    )
}
fn user_home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("user home is unavailable")
}
fn absolute(path: &Path) -> Result<PathBuf> {
    // Windows drive-relative paths (C:profiles) need the native drive's current
    // directory; joining the process cwd leaves them relative. This resolves
    // them without requiring the settings file or directory to exist.
    let path = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(part.as_os_str()),
        }
    }
    Ok(normalized)
}
pub fn target_path(provider: &str, root: &Path) -> Result<PathBuf> {
    let (variable, name) = match provider {
        "codex" => ("CODEX_HOME", "config.toml"),
        "claude" => ("CLAUDE_CONFIG_DIR", "settings.json"),
        _ => bail!("client must be codex or claude"),
    };
    let mut root = root.to_owned();
    if root.as_os_str().is_empty() {
        root = std::env::var_os(variable)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or(user_home()?.join(format!(".{provider}")));
    }
    if let Ok(rest) = root.strip_prefix("~") {
        root = user_home()?.join(rest);
    }
    absolute(&root.join(name))
}
pub fn default_root(provider: &str) -> Result<PathBuf> {
    Ok(target_path(provider, Path::new(""))?
        .parent()
        .context("missing settings directory")?
        .to_owned())
}
pub fn discover(home: &Path) -> Result<Vec<Target>> {
    let store = load(home)?;
    let user = user_home()?;
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for provider in ["codex", "claude"] {
        for (root, required) in [
            (default_root(provider)?, true),
            (user.join(format!(".{provider}")), false),
            (user.join(".gg").join(provider), false),
        ] {
            if !required && !root.exists() {
                continue;
            }
            let path = target_path(provider, &root)?.to_string_lossy().into_owned();
            if !seen.insert(path.clone()) {
                continue;
            }
            let entry = store.entries.iter().find(|v| v.path == path);
            targets.push(Target {
                provider: provider.into(),
                exists: Path::new(&path).exists(),
                active: entry.is_some_and(|v| v.active),
                registered: entry.is_some(),
                path,
            });
        }
    }
    for entry in store.entries {
        if seen.insert(entry.path.clone()) {
            targets.push(Target {
                provider: entry.provider,
                path: entry.path,
                exists: true,
                active: entry.active,
                registered: true,
            });
        }
    }
    Ok(targets)
}
pub fn sorted_targets(targets: &mut [Target]) {
    targets.sort_by(|a, b| (&a.provider, &a.path).cmp(&(&b.provider, &b.path)));
}

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let info = match fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !info.is_file() {
        bail!("refusing nonregular settings file: {}", path.display());
    }
    if info.len() > 8 << 20 {
        bail!("settings file too large");
    }
    let data = fs::read(path)?;
    if data.len() > 8 << 20 {
        bail!("settings file too large");
    }
    Ok(Some(data))
}
// Unlike config::write, this must never chmod a native client's directory.
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing settings directory")?;
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(parent)?;
    match fs::symlink_metadata(path) {
        Ok(info) if !info.is_file() => {
            bail!("refusing nonregular settings file {}", path.display())
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let temporary = parent.join(format!(".noria-settings-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
pub fn preview(
    home: &Path,
    provider: &str,
    root: &Path,
    executable: &Path,
    address: &str,
) -> Result<Value> {
    let path = target_path(provider, root)?;
    let bytes = read(&path)?.unwrap_or_default();
    for entry in load(home)?.entries {
        if Path::new(&entry.path) == path && entry.active {
            check(&entry)?;
            return Ok(
                json!({"provider":provider,"path":path,"changes":changes(provider,home,executable,address),"note":"Already configured. Removing restores the original routing fields without changing native login."}),
            );
        }
    }
    let mut entry = Entry {
        provider: provider.into(),
        path: path.to_string_lossy().into_owned(),
        ..Entry::default()
    };
    activate(home, executable, address, &bytes, &mut entry)?;
    Ok(
        json!({"provider":provider,"path":path,"changes":changes(provider,home,executable,address),"note":"Backs up this file; preserves other settings and native login. Stop restores direct settings. Existing clients may need restart."}),
    )
}
pub fn install(
    home: &Path,
    provider: &str,
    root: &Path,
    executable: &Path,
    address: &str,
) -> Result<()> {
    let _lock = config::lock(home, "integrations")?;
    let mut store = load(home)?;
    let path = target_path(provider, root)?;
    let accounts = absolute(&home.join("accounts"))?;
    let parent = path.parent().context("missing settings directory")?;
    if parent.starts_with(&accounts)
        || parent
            .canonicalize()
            .ok()
            .zip(accounts.canonicalize().ok())
            .is_some_and(|(p, a)| p.starts_with(a))
    {
        bail!("cannot integrate an upstream Noria account profile");
    }
    if let Some(entry) = store
        .entries
        .iter()
        .find(|v| Path::new(&v.path) == path && v.active)
    {
        return check(entry);
    }
    let before = read(&path)?;
    let mut entry = Entry {
        provider: provider.into(),
        path: path.to_string_lossy().into_owned(),
        created: before.is_none(),
        ..Entry::default()
    };
    let after = activate(
        home,
        executable,
        address,
        before.as_deref().unwrap_or_default(),
        &mut entry,
    )?;
    let digest = Sha256::digest(entry.path.as_bytes());
    let stem: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    entry.backup = home
        .join("backups")
        .join(format!("{stem}.original"))
        .to_string_lossy()
        .into_owned();
    match fs::metadata(&entry.backup) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => config::write(
            Path::new(&entry.backup),
            before.as_deref().unwrap_or_default(),
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let old = store.clone();
    if let Some(previous) = store.entries.iter_mut().find(|v| v.path == entry.path) {
        *previous = entry;
    } else {
        store.entries.push(entry);
    }
    save(home, &store)?;
    if let Err(error) = write(&path, &after) {
        let _ = save(home, &old);
        return Err(error);
    }
    Ok(())
}
pub fn set_active(home: &Path, executable: &Path, address: &str, active: bool) -> Result<()> {
    let _lock = config::lock(home, "integrations")?;
    let mut store = load(home)?;
    let mut changes = Vec::new();
    for entry in &mut store.entries {
        if entry.active == active {
            if active {
                check(entry).with_context(|| entry.path.clone())?;
            }
            continue;
        }
        let before = read(Path::new(&entry.path))?;
        let bytes = before.as_deref().unwrap_or_default();
        let after = if active {
            activate(home, executable, address, bytes, entry)
        } else {
            deactivate(bytes, entry)
        }
        .with_context(|| entry.path.clone())?;
        changes.push((PathBuf::from(&entry.path), before, after));
    }
    let rollback = |count: usize| {
        for (path, before, _) in changes[..count].iter().rev() {
            match before {
                Some(bytes) => {
                    let _ = write(path, bytes);
                }
                None => {
                    let _ = fs::remove_file(path);
                }
            }
        }
    };
    for (index, (path, _, after)) in changes.iter().enumerate() {
        if let Err(error) = write(path, after) {
            rollback(index);
            return Err(error);
        }
    }
    if let Err(error) = save(home, &store) {
        rollback(changes.len());
        return Err(error);
    }
    Ok(())
}
pub fn remove(home: &Path, provider: &str, root: &Path) -> Result<()> {
    let _lock = config::lock(home, "integrations")?;
    let path = target_path(provider, root)?;
    let mut store = load(home)?;
    if let Some(index) = store
        .entries
        .iter()
        .position(|v| Path::new(&v.path) == path)
    {
        let before = read(&path)?;
        if store.entries[index].active {
            let after = deactivate(
                before.as_deref().unwrap_or_default(),
                &mut store.entries[index],
            )?;
            write(&path, &after)?;
        }
        store.entries.remove(index);
        if let Err(error) = save(home, &store) {
            if let Some(bytes) = before {
                let _ = write(&path, &bytes);
            }
            return Err(error);
        }
    }
    Ok(())
}
pub fn check(entry: &Entry) -> Result<()> {
    let bytes = read(Path::new(&entry.path))?.unwrap_or_default();
    if entry.active {
        deactivate(&bytes, &mut entry.clone())?;
    }
    Ok(())
}
pub fn views(home: &Path) -> Result<Vec<View>> {
    Ok(load(home)?
        .entries
        .into_iter()
        .map(|entry| {
            let result = check(&entry);
            View {
                provider: entry.provider,
                path: entry.path,
                active: entry.active,
                healthy: result.is_ok(),
                error: result.err().map(|e| e.to_string()).unwrap_or_default(),
            }
        })
        .collect())
}
pub fn helper_command(executable: &Path, home: &Path) -> String {
    let exe = executable.to_string_lossy();
    let home = home.to_string_lossy();
    if cfg!(windows) {
        format!("\"{exe}\" auth-token --home \"{home}\"")
    } else {
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\"'\"'"));
        format!("{} auth-token --home {}", quote(&exe), quote(&home))
    }
}
fn changes(provider: &str, home: &Path, exe: &Path, address: &str) -> Value {
    if provider == "codex" {
        json!({"model_provider":"noria","model_providers":{"noria":{"name":"Noria","base_url":format!("http://{address}/codex/v1"),"wire_api":"responses","supports_websockets":true,"auth":{"command":exe,"args":["auth-token","--home",home]}}}})
    } else {
        json!({"apiKeyHelper":helper_command(exe,home),"env":{"ANTHROPIC_BASE_URL":format!("http://{address}/claude"),"ENABLE_TOOL_SEARCH":"true","ANTHROPIC_AUTH_TOKEN":"","ANTHROPIC_API_KEY":"","CLAUDE_CODE_OAUTH_TOKEN":""}})
    }
}
fn activate(
    home: &Path,
    exe: &Path,
    address: &str,
    bytes: &[u8],
    entry: &mut Entry,
) -> Result<Vec<u8>> {
    if entry.provider == "codex" {
        return codex::activate(home, exe, address, bytes, entry);
    }
    let mut root: Value = if bytes.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(bytes).context("invalid settings JSON")?
    };
    if !root.is_object() {
        bail!("settings must be a JSON object");
    }
    entry.fields.clear();
    let replacements = changes(&entry.provider, home, exe, address);
    for path in [
        vec!["apiKeyHelper"],
        vec!["env", "ANTHROPIC_BASE_URL"],
        vec!["env", "ENABLE_TOOL_SEARCH"],
        vec!["env", "ANTHROPIC_AUTH_TOKEN"],
        vec!["env", "ANTHROPIC_API_KEY"],
        vec!["env", "CLAUDE_CODE_OAUTH_TOKEN"],
    ] {
        let path: Vec<String> = path.into_iter().map(String::from).collect();
        let previous = get(&root, &path).cloned();
        let installed = get(&replacements, &path)
            .context("missing integration field")?
            .clone();
        entry.fields.push(Field {
            path: path.clone(),
            before: previous.clone().unwrap_or(Value::Null),
            existed: previous.is_some(),
            installed: installed.clone(),
        });
        put(&mut root, &path, Some(installed))?;
    }
    entry.active = true;
    json_bytes(&root)
}
fn deactivate(bytes: &[u8], entry: &mut Entry) -> Result<Vec<u8>> {
    if entry.provider == "codex" {
        return codex::deactivate(bytes, entry);
    }
    let mut root: Value = serde_json::from_slice(bytes)?;
    for field in &entry.fields {
        if get(&root, &field.path) != Some(&field.installed) {
            bail!(
                "managed {} was edited externally; refusing to overwrite",
                field.path.join(".")
            );
        }
    }
    for field in &entry.fields {
        put(
            &mut root,
            &field.path,
            field.existed.then(|| field.before.clone()),
        )?;
    }
    if root
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        root.as_object_mut()
            .context("settings must be a JSON object")?
            .remove("env");
    }
    entry.active = false;
    json_bytes(&root)
}
fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}
fn get<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter()
        .try_fold(root, |value, key| value.as_object()?.get(key))
}
fn put(root: &mut Value, path: &[String], value: Option<Value>) -> Result<()> {
    let mut cursor = root;
    for key in &path[..path.len() - 1] {
        let object = cursor
            .as_object_mut()
            .context("settings must be a JSON object")?;
        cursor = object.entry(key).or_insert_with(|| json!({}));
        if !cursor.is_object() {
            bail!("{key} must be a table/object");
        }
    }
    let object = cursor
        .as_object_mut()
        .context("settings must be a JSON object")?;
    let key = path.last().context("missing field path")?;
    if let Some(value) = value {
        object.insert(key.clone(), value);
    } else {
        object.remove(key);
    }
    Ok(())
}
