//! Exercise the actual Rust executable against private fake profiles only.
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let home = root.join("pool");
        Self {
            _temp: temp,
            root,
            home,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_noria"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("NORIA_HOME", &self.home)
            .env("CODEX_HOME", self.root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("APPDATA", self.root.join("appdata"))
            .env("LOCALAPPDATA", self.root.join("localappdata"));
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.run(args).stdout).unwrap()
    }
    fn init(&self) {
        self.run(&["init"]);
        let mut c: Value =
            serde_json::from_slice(&fs::read(self.home.join("config.json")).unwrap()).unwrap();
        c["auto_update"] = json!(false);
        fs::write(
            self.home.join("config.json"),
            serde_json::to_vec(&c).unwrap(),
        )
        .unwrap();
    }
    fn account(&self, id: &str, email: &str, alias: &str) {
        let path = self.home.join("accounts").join(id);
        fs::create_dir_all(&path).unwrap();
        let config = self.home.join("config.json");
        let mut c: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        c["accounts"].as_array_mut().unwrap().push(json!({"id":id,"email":email,"alias":alias,"provider":"codex","profile":path,"authenticated":true,"enabled":false,"last_login":"0001-01-01T00:00:00Z"}));
        fs::write(config, serde_json::to_vec(&c).unwrap()).unwrap();
    }
}
#[test]
fn help_and_version_do_not_initialize_or_read_accounts() {
    let f = Fixture::new();
    assert_eq!(
        String::from_utf8_lossy(&f.run(&["--version"]).stdout).trim(),
        format!("noria {}", noria::VERSION)
    );
    assert!(String::from_utf8_lossy(&f.run(&["--help"]).stdout).contains("start | stop | restart"));
    assert!(!f.home.exists());
    assert!(!f.command().arg("serve").output().unwrap().status.success());
    assert!(!f.home.exists());
}
#[test]
fn initialization_is_idempotent_and_global_home_works_after_subcommand() {
    let f = Fixture::new();
    let selected = f.root.join("other");
    f.run(&["init", "--home", selected.to_str().unwrap()]);
    let before = fs::read(selected.join("secret")).unwrap();
    f.run(&["--home", selected.to_str().unwrap(), "init"]);
    assert_eq!(before, fs::read(selected.join("secret")).unwrap());
    assert!(!f.home.exists());
    let token = f.run(&["auth-token", "--home", selected.to_str().unwrap()]);
    assert_eq!(
        String::from_utf8_lossy(&token.stdout).trim().as_bytes(),
        before
    );
}
#[test]
fn swift_dashboard_schema_and_nanosecond_windows_read_go_state() {
    let f = Fixture::new();
    f.init();
    f.account("acc_a", "a@example.test", "");
    let state = json!({"version":1,"bindings":{"codex:old-thread":"acc_a"},"activity":null,"blockers":null,"snapshots":{"acc_a":{"account_id":"acc_a","observed_at":"2026-01-01T00:00:00Z","available":true,"windows":[{"name":"five_hour","used_percent":25.0,"duration":18000000000000i64,"resets_at":"2026-01-01T05:00:00Z"}]}},"learning":null,"completed":null,"completed_models":null,"predicted_resets":null,"window_observed":null});
    let path = f.home.join("scheduler.json");
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();
    let dashboard = f.json(&["status", "--json"]);
    assert_eq!(dashboard["schema_version"], 1);
    assert_eq!(dashboard["running"], false);
    assert_eq!(dashboard["bindings"], 1);
    assert!(dashboard["preferences"]["auto_update"].is_boolean());
    assert!(dashboard["client_roots"].is_array());
    assert!(dashboard["integrations"].is_array());
    let account = &dashboard["accounts"][0];
    assert_eq!(account["id"], "acc_a");
    assert_eq!(account["display_name"], "a@example.test");
    assert_eq!(
        account["snapshot"]["windows"][0]["duration"],
        18000000000000i64
    );
    assert!(account.get("profile").is_none());
    assert_eq!(
        before,
        fs::read(path).unwrap(),
        "offline status must not rewrite scheduler state"
    );
}
#[test]
fn account_aliases_do_not_change_ids_and_ambiguous_email_is_rejected() {
    let f = Fixture::new();
    f.init();
    f.account("acc_a", "same@example.test", "");
    f.account("acc_b", "same@example.test", "");
    assert!(
        !f.command()
            .args(["account", "rename", "same@example.test", "Oops"])
            .output()
            .unwrap()
            .status
            .success()
    );
    f.run(&["account", "rename", "acc_a", "Work", "--json"]);
    let accounts = f.json(&["account", "list", "--json"]);
    assert_eq!(accounts[0]["id"], "acc_a");
    assert_eq!(accounts[0]["display_name"], "Work");
    assert!(accounts[0].get("profile").is_none());
    f.run(&["account", "rename", "acc_a", "--json"]);
    assert_eq!(
        f.json(&["account", "list", "--json"])[0]["display_name"],
        "same@example.test"
    );
}
#[test]
fn integration_preview_never_edits_native_profiles_or_starts_service() {
    let f = Fixture::new();
    f.init();
    let client = f.root.join("codex");
    fs::create_dir_all(&client).unwrap();
    let original = "# private comment\nmodel = \"fixture\"\n";
    fs::write(client.join("config.toml"), original).unwrap();
    let preview = f.json(&[
        "integrate",
        "codex",
        "--client-home",
        client.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(
        Path::new(preview["path"].as_str().unwrap()),
        client.join("config.toml")
    );
    assert_eq!(
        fs::read_to_string(client.join("config.toml")).unwrap(),
        original
    );
    assert!(!f.home.join("daemon.json").exists());
    assert!(!f.home.join("integrations.json").exists());
}
#[test]
fn env_overrides_preserve_native_homes_and_clear_conflicting_credentials() {
    let f = Fixture::new();
    f.init();
    let output = String::from_utf8(f.run(&["env", "codex"]).stdout).unwrap();
    assert!(output.contains("supports_websockets=true"));
    assert!(output.contains("unset OPENAI_API_KEY"));
    assert!(output.contains("CLAUDE_CODE_USE_BEDROCK"));
    assert!(!output.contains("export CODEX_HOME"));
    let output = String::from_utf8(f.run(&["env", "claude"]).stdout).unwrap();
    assert!(output.contains("ENABLE_TOOL_SEARCH='true'"));
    assert!(!output.contains("export CLAUDE_CONFIG_DIR"));
}
