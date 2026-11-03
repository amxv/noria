use super::*;
use tempfile::TempDir;

const ADDRESS: &str = "127.0.0.1:43119";
fn fixture(provider: &str, body: &str) -> (TempDir, TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    config::init(home.path()).unwrap();
    let path = target_path(provider, root.path()).unwrap();
    fs::write(&path, body).unwrap();
    (home, root, path)
}
fn exe() -> &'static Path {
    Path::new("/safe/noria")
}

#[test]
fn codex_round_trip_keeps_comments_native_login_and_unrelated_edits() {
    let original = "# my settings\nmodel_provider = \"openai\" # preferred\nmodel = \"fixture-model\"\n[features]\nsome_feature = true\n";
    let (home, root, path) = fixture("codex", original);
    let auth = root.path().join("auth.json");
    fs::write(&auth, "login-sentinel").unwrap();
    preview(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    install(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    let installed = fs::read_to_string(&path).unwrap();
    assert_eq!(
        installed.parse::<toml_edit::DocumentMut>().unwrap()["model_provider"].as_str(),
        Some("noria")
    );
    assert!(installed.contains("# my settings"));
    fs::write(
        &path,
        installed.replace(
            "some_feature = true",
            "some_feature = false # edited while using Noria",
        ),
    )
    .unwrap();
    set_active(home.path(), exe(), ADDRESS, false).unwrap();
    let restored = fs::read_to_string(&path).unwrap();
    assert_eq!(
        restored,
        original.replace(
            "some_feature = true",
            "some_feature = false # edited while using Noria"
        )
    );
    assert_eq!(fs::read_to_string(auth).unwrap(), "login-sentinel");
    set_active(home.path(), exe(), ADDRESS, true).unwrap();
    remove(home.path(), "codex", root.path()).unwrap();
    assert!(load(home.path()).unwrap().entries.is_empty());
    assert_eq!(fs::read_to_string(path).unwrap(), restored);
}
#[test]
fn codex_array_tables_restore_exact_original_bytes() {
    let original = r#"# regular Codex config with nested tables and arrays
model = "gpt-6"
model_provider = "openai" # keep my direct login
notify = ["echo", "ready"]

[features]
multi_agent = true

[skills]
enabled = true

[[skills.config]]
path = "/tmp/skill-alpha"
enabled = true

[[skills.config]]
path = "/tmp/skill-beta"
enabled = false

[hooks]
enabled = true
"#;
    let (home, root, path) = fixture("codex", original);
    preview(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    install(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    set_active(home.path(), exe(), ADDRESS, false).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    set_active(home.path(), exe(), ADDRESS, true).unwrap();
    remove(home.path(), "codex", root.path()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}
#[test]
fn claude_restores_auth_overrides_and_retains_new_settings() {
    let original = r#"{"apiKeyHelper":"original-helper","env":{"ANTHROPIC_BASE_URL":"https://original.invalid","ANTHROPIC_AUTH_TOKEN":"old-token","ANTHROPIC_API_KEY":"old-key","CLAUDE_CODE_OAUTH_TOKEN":"old-oauth","OTHER":"sentinel"},"theme":"dark"}"#;
    let (home, root, path) = fixture("claude", original);
    install(home.path(), "claude", root.path(), exe(), ADDRESS).unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for key in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        assert_eq!(value["env"][key], "");
    }
    value["theme"] = json!("light");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    set_active(home.path(), exe(), ADDRESS, false).unwrap();
    let mut expected: Value = serde_json::from_str(original).unwrap();
    expected["theme"] = json!("light");
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
        expected
    );
}
#[test]
fn external_routing_edits_never_get_overwritten() {
    for provider in ["codex", "claude"] {
        let (home, root, path) = fixture(provider, if provider == "codex" { "" } else { "{}" });
        install(home.path(), provider, root.path(), exe(), ADDRESS).unwrap();
        let changed = fs::read_to_string(&path)
            .unwrap()
            .replace("http://127.0.0.1:43119", "https://other-gateway.invalid");
        fs::write(&path, &changed).unwrap();
        assert!(set_active(home.path(), exe(), ADDRESS, false).is_err());
        assert!(remove(home.path(), provider, root.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
        assert!(!views(home.path()).unwrap()[0].healthy);
    }
}
#[test]
fn all_targets_validate_before_any_mutation() {
    let (home, root, path) = fixture("codex", "# direct\n");
    let claude = tempfile::tempdir().unwrap();
    install(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    install(home.path(), "claude", claude.path(), exe(), ADDRESS).unwrap();
    let before = fs::read(&path).unwrap();
    let claude_path = target_path("claude", claude.path()).unwrap();
    let changed = fs::read_to_string(&claude_path)
        .unwrap()
        .replace("http://127.0.0.1:43119", "https://external.invalid");
    fs::write(&claude_path, changed).unwrap();
    assert!(set_active(home.path(), exe(), ADDRESS, false).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(load(home.path()).unwrap().entries.iter().all(|v| v.active));
}
#[test]
fn idempotent_install_preserves_backup_and_never_embeds_gateway_secret() {
    let (home, root, path) = fixture("codex", "# sentinel\n");
    let secret = config::secret(home.path()).unwrap();
    install(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    let before = fs::read_to_string(&path).unwrap();
    assert!(!before.contains(&secret));
    assert!(before.contains("auth-token"));
    install(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    let store = load(home.path()).unwrap();
    assert_eq!(store.entries.len(), 1);
    assert_eq!(
        fs::read_to_string(&store.entries[0].backup).unwrap(),
        "# sentinel\n"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), before);
}
#[test]
fn upstream_account_profiles_and_invalid_settings_are_refused() {
    let home = tempfile::tempdir().unwrap();
    config::init(home.path()).unwrap();
    let account = home.path().join("accounts/fixture");
    fs::create_dir_all(&account).unwrap();
    assert!(install(home.path(), "claude", &account, exe(), ADDRESS).is_err());
    for original in ["null", "[]", r#"{"env":"invalid"}"#] {
        let (home, root, path) = fixture("claude", original);
        assert!(install(home.path(), "claude", root.path(), exe(), ADDRESS).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
    let (home, root, _) = fixture("codex", "\"model_provider\" = \"openai\"\n");
    assert!(install(home.path(), "codex", root.path(), exe(), ADDRESS).is_err());
}
#[cfg(unix)]
#[test]
fn native_directory_permissions_and_symlinks_are_respected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (home, root, path) = fixture("codex", "");
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    install(home.path(), "codex", root.path(), exe(), ADDRESS).unwrap();
    assert_eq!(
        fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    remove(home.path(), "codex", root.path()).unwrap();
    fs::remove_file(&path).unwrap();
    let other = root.path().join("other");
    fs::write(&other, "sentinel").unwrap();
    symlink(&other, &path).unwrap();
    assert!(install(home.path(), "codex", root.path(), exe(), ADDRESS).is_err());
    assert_eq!(fs::read_to_string(other).unwrap(), "sentinel");
}
