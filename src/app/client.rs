use super::*;
use std::process::Stdio;

pub async fn integrate(home: &Path, mut args: Vec<String>) -> Result<()> {
    if args.is_empty() || help_requested(&args) {
        println!(
            "Usage: noria integrate codex|claude [--client-home path] [--yes] [--remove] [--json]\nPreview changes by default. --yes authorizes a reversible settings edit. Restart existing clients after switching routing."
        );
        return Ok(());
    }
    let kind = args.remove(0);
    config::init(home)?;
    if kind == "claude-desktop" {
        no_args(&args, "noria integrate claude-desktop")?;
        let c = config::load(home)?;
        return print_json(
            &json!({"inferenceProvider":"gateway","inferenceGatewayBaseUrl":format!("http://{}/claude",c.listen),"inferenceGatewayAuthScheme":"bearer","inferenceCredentialKind":"helper-script","inferenceCredentialHelper":service::executable()?,"inferenceCredentialHelperArgs":["auth-token","--home",home.to_string_lossy()]}),
        );
    }
    let root = PathBuf::from(option(&mut args, "--client-home")?.unwrap_or_default());
    let yes = flag(&mut args, "--yes");
    let remove = flag(&mut args, "--remove");
    let as_json = flag(&mut args, "--json");
    no_args(
        &args,
        "noria integrate codex|claude [--client-home path] [--yes] [--remove] [--json]",
    )?;
    let c = config::load(home)?;
    let exe = service::executable()?;
    let address = daemon_address(home, &c.listen);
    let plan = integration::preview(home, &kind, &root, &exe, &address)?;
    if !yes {
        if as_json {
            return print_json(&plan);
        }
        println!(
            "Settings file: {}",
            plan["path"].as_str().unwrap_or_default()
        );
        println!("{}", plan["note"].as_str().unwrap_or_default());
        print_json(&plan["changes"])?;
        println!("Run again with --yes to apply.");
        return Ok(());
    }
    if remove {
        integration::remove(home, &kind, &root)?;
    } else {
        if !running(home).await {
            let _lock = config::lock(home, "lifecycle")?;
            start(home).await?;
        }
        integration::install(home, &kind, &root, &exe, &daemon_address(home, &c.listen))?;
    }
    if as_json {
        return print_json(
            &json!({"path":plan["path"],"configured":!remove,"active":service::read_intent(home).wanted&&!remove,"client_restart_required":true}),
        );
    }
    println!("Updated {}", plan["path"].as_str().unwrap_or_default());
    println!("Reopen existing clients to pick up routing.");
    Ok(())
}
pub fn integrations(home: &Path, mut args: Vec<String>) -> Result<()> {
    if help_requested(&args) {
        println!("Usage: noria integrations [--json]");
        return Ok(());
    }
    let as_json = flag(&mut args, "--json");
    no_args(&args, "noria integrations [--json]")?;
    let views = integration::views(home)?;
    if as_json {
        return print_json(&views);
    }
    for v in views {
        println!(
            "{}\t{}\t{}",
            v.provider,
            v.path,
            if !v.healthy {
                "changed outside Noria"
            } else if v.active {
                "Noria"
            } else {
                "direct"
            }
        );
    }
    Ok(())
}
pub(super) fn native_roots() -> Vec<Value> {
    let mut roots = Vec::new();
    for kind in ["codex", "claude"] {
        if let Ok(root) = integration::default_root(kind) {
            roots.push(json!({"provider":kind,"root":root}));
            if let Ok(user) = service::user_home() {
                let custom = user.join(".gg").join(kind);
                if custom != root && custom.is_dir() {
                    roots.push(json!({"provider":kind,"root":custom}));
                }
            }
        }
    }
    roots
}
const CLEARED: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "NORIA_GATEWAY_TOKEN",
    "ENABLE_TOOL_SEARCH",
];
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
pub(super) fn codex_overrides(address: &str) -> Vec<String> {
    vec![
        "-c".into(),
        "model_provider=\"noria\"".into(),
        "-c".into(),
        format!(
            "model_providers.noria={{name=\"Noria\",base_url={},env_key=\"NORIA_GATEWAY_TOKEN\",wire_api=\"responses\",supports_websockets=true}}",
            serde_json::to_string(&format!("http://{address}/codex/v1")).unwrap()
        ),
    ]
}
pub async fn command(home: &Path, op: &str, mut args: Vec<String>) -> Result<()> {
    if args.is_empty() || is_help(&args[0]) {
        println!(
            "Usage: noria env codex|claude\n       noria run codex|claude -- [args]\nRetain your client home, history, skills and settings. Routing changes apply to the child only.\nThe env command intentionally prints the local gateway credential."
        );
        return Ok(());
    }
    let kind = args.remove(0);
    ensure!(
        matches!(kind.as_str(), "codex" | "claude"),
        "client must be codex or claude"
    );
    if op == "env" {
        no_args(&args, "noria env codex|claude")?;
    }
    let c = config::load(home)?;
    let secret = config::secret(home)?;
    let address = daemon_address(home, &c.listen);
    let mut exported = std::collections::BTreeMap::new();
    let overrides = if kind == "codex" {
        exported.insert("NORIA_GATEWAY_TOKEN", secret);
        codex_overrides(&address)
    } else {
        exported.insert("ANTHROPIC_BASE_URL", format!("http://{address}/claude"));
        exported.insert("ANTHROPIC_AUTH_TOKEN", secret);
        exported.insert("ENABLE_TOOL_SEARCH", "true".into());
        Vec::new()
    };
    if op == "env" {
        println!("unset {}", CLEARED.join(" "));
        for (key, value) in exported {
            println!("export {key}={}", shell_quote(&value));
        }
        if kind == "codex" {
            println!(
                "codex() {{ command codex {} \"$@\"; }}",
                overrides
                    .iter()
                    .map(|s| shell_quote(s))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--") {
        args.remove(0);
    }
    let mut command = tokio::process::Command::new(&kind);
    command
        .args(overrides)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for key in CLEARED {
        command.env_remove(key);
    }
    command.envs(exported);
    let status = command
        .status()
        .await
        .with_context(|| format!("start {kind}"))?;
    ensure!(status.success(), "{kind} exited with {status}");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_overrides_keep_native_home_and_websocket_support() {
        let args = codex_overrides("127.0.0.1:43119");
        assert_eq!(args.len(), 4);
        assert!(args[3].contains("supports_websockets=true"));
        assert!(!args.join(" ").contains("CODEX_HOME"));
        assert!(args[3].contains("NORIA_GATEWAY_TOKEN"));
    }
    #[test]
    fn shell_quoting_is_literal() {
        assert_eq!(shell_quote("a'b$(`x`)"), "'a'\"'\"'b$(`x`)'");
    }
}
