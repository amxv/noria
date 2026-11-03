use super::*;
use crate::{model::Account, provider::Manager};
use chrono::Utc;

pub async fn command(home: &Path, mut args: Vec<String>) -> Result<()> {
    if args.is_empty() || help_requested(&args) {
        println!(
            "Usage: noria account add codex|claude [--alias NAME] [--json]\n       noria account list [--json]\n       noria account relogin|enable|disable|remove ACCOUNT [--json]\n       noria account rename ACCOUNT [ALIAS] [--json]\nAccounts display their email by default. ACCOUNT accepts an ID, unique email, or unique alias."
        );
        return Ok(());
    }
    let as_json = flag(&mut args, "--json");
    config::init(home)?;
    let op = args.remove(0);
    if op == "list" {
        no_args(&args, "noria account list [--json]")?;
        let c = config::load(home)?;
        if as_json {
            return print_json(&c.accounts.iter().map(safe).collect::<Vec<_>>());
        }
        for a in c.accounts {
            println!(
                "{}\t{}\tenabled={}\temail={}\tid={}",
                a.display_name(),
                a.provider,
                a.enabled,
                a.email,
                a.id
            );
        }
        return Ok(());
    }
    ensure!(
        !args.is_empty(),
        "account command requires a provider or account"
    );
    if op == "add" {
        let kind = args.remove(0);
        ensure!(
            matches!(kind.as_str(), "codex" | "claude"),
            "usage: noria account add codex|claude [--alias NAME] [--json]"
        );
        let alias = option(&mut args, "--alias")?.unwrap_or_default();
        no_args(&args, "noria account add codex|claude [--alias NAME]")?;
        validate_alias(&alias)?;
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("secure account ID: {e}"))?;
        let id = format!(
            "acc_{}",
            random
                .iter()
                .map(|v| format!("{v:02x}"))
                .collect::<String>()
        );
        let account = Account {
            id: id.clone(),
            provider: kind,
            profile: home
                .join("accounts")
                .join(&id)
                .to_string_lossy()
                .into_owned(),
            alias,
            enabled: false,
            ..Default::default()
        };
        config::private_dir(Path::new(&account.profile))?;
        config::mutate(home, |c| {
            c.accounts.push(account.clone());
            Ok(())
        })?;
        if !as_json {
            println!(
                "Opening native {} sign-in in your browser…",
                account.provider
            );
        }
        complete_login(home, &account).await?;
        let a = find(&config::load(home)?, &id)?.clone();
        if as_json {
            return print_json(&json!({"ok":true,"account":safe(&a)}));
        }
        println!("Added {}", a.display_name());
        return Ok(());
    }
    let reference = args.remove(0);
    let c = config::load(home)?;
    let a = resolve(&c, &reference)?.clone();
    match op.as_str() {
        "relogin" => {
            no_args(&args, "noria account relogin ACCOUNT [--json]")?;
            if !as_json {
                println!("Opening sign-in for {}", a.display_name());
            }
            complete_login(home, &a).await?;
            let a = find(&config::load(home)?, &a.id)?.clone();
            if as_json {
                return print_json(&json!({"ok":true,"account":safe(&a)}));
            }
            println!("Signed in: {}", a.display_name());
        }
        "rename" => {
            ensure!(
                args.len() <= 1,
                "usage: noria account rename ACCOUNT [ALIAS]"
            );
            let alias = args.pop().unwrap_or_default();
            validate_alias(&alias)?;
            config::mutate(home, |c| {
                find_mut(c, &a.id)?.alias = alias;
                Ok(())
            })?;
            reload(home).await;
            if as_json {
                return print_json(&json!({"ok":true}));
            }
            println!("Account display name updated");
        }
        "enable" | "disable" | "remove" => {
            no_args(&args, &format!("noria account {op} ACCOUNT [--json]"))?;
            config::mutate(home, |c| {
                let account = find_mut(c, &a.id)?;
                if op == "enable" {
                    ensure!(
                        account.authenticated,
                        "sign in to the account before enabling it"
                    );
                    account.enabled = true;
                } else if op == "disable" {
                    account.enabled = false;
                } else {
                    c.accounts.retain(|v| v.id != a.id);
                }
                Ok(())
            })?;
            reload(home).await;
            if as_json {
                return print_json(&json!({"ok":true}));
            }
            println!("Account {} {op}", a.display_name());
        }
        _ => bail!("unknown account command"),
    }
    Ok(())
}
pub(super) fn find<'a>(c: &'a config::Config, id: &str) -> Result<&'a Account> {
    c.accounts
        .iter()
        .find(|a| a.id == id)
        .with_context(|| format!("account {id} does not exist"))
}
fn find_mut<'a>(c: &'a mut config::Config, id: &str) -> Result<&'a mut Account> {
    c.accounts
        .iter_mut()
        .find(|a| a.id == id)
        .context("account removed before modification")
}
pub(super) fn resolve<'a>(c: &'a config::Config, query: &str) -> Result<&'a Account> {
    if let Some(account) = c.accounts.iter().find(|a| a.id == query) {
        return Ok(account);
    }
    let matches: Vec<_> = c
        .accounts
        .iter()
        .filter(|a| {
            (!a.email.is_empty() && a.email.eq_ignore_ascii_case(query))
                || (!a.alias.is_empty() && a.alias == query)
        })
        .collect();
    ensure!(
        matches.len() <= 1,
        "account reference is ambiguous; use its stable ID from account list --json"
    );
    matches
        .first()
        .copied()
        .with_context(|| format!("account {query:?} not found"))
}
fn validate_alias(alias: &str) -> Result<()> {
    ensure!(
        alias.len() <= 120 && !alias.contains(['\r', '\n', '\0', '\t']),
        "alias must be at most 120 characters on one line"
    );
    Ok(())
}
fn safe(a: &Account) -> Value {
    json!({"id":a.id,"provider":a.provider,"email":a.email,"alias":a.alias,"display_name":a.display_name(),"enabled":a.enabled,"authenticated":a.authenticated})
}
async fn complete_login(home: &Path, account: &Account) -> Result<()> {
    config::mutate(home, |c| {
        let a = find_mut(c, &account.id)?;
        a.enabled = false;
        a.authenticated = false;
        Ok(())
    })?;
    reload(home).await;
    let manager = Manager::new(home);
    tokio::select! {result=manager.login(account)=>result?,_=daemon::shutdown_signal()=>bail!("sign-in cancelled")};
    let identity = manager.identity(account).await?;
    ensure!(identity.authenticated, "provider did not confirm sign-in");
    ensure!(
        !identity.email.is_empty(),
        "signed in, but this client version did not return an account email; update the native client and retry"
    );
    ensure!(
        account.email.is_empty() || account.email.eq_ignore_ascii_case(&identity.email),
        "a different account was selected; this entry remains disabled. Add that account separately, or sign in again with the original email"
    );
    config::mutate(home, |c| {
        for other in &c.accounts {
            ensure!(
                other.id == account.id
                    || other.provider != account.provider
                    || !other.authenticated
                    || !other.email.eq_ignore_ascii_case(&identity.email),
                "{} is already in the pool as {}; relogin that account instead of counting its quota twice",
                identity.email,
                other.id
            );
        }
        let a = find_mut(c, &account.id)?;
        a.email = identity.email;
        a.authenticated = true;
        a.enabled = true;
        a.last_login = Some(Utc::now());
        Ok(())
    })?;
    reload(home).await;
    let _ = daemon::refresh_account(home, &account.id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_id_wins_and_ambiguous_email_requires_id() {
        let c = config::Config {
            accounts: vec![
                Account {
                    id: "a".into(),
                    email: "same@example.test".into(),
                    ..Default::default()
                },
                Account {
                    id: "b".into(),
                    email: "same@example.test".into(),
                    alias: "a".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(resolve(&c, "a").unwrap().id, "a");
        assert!(resolve(&c, "same@example.test").is_err());
    }
    #[test]
    fn aliases_and_safe_account_output() {
        assert!(validate_alias("bad\nname").is_err());
        let a = Account {
            profile: "sensitive/path".into(),
            ..Default::default()
        };
        assert!(safe(&a).get("profile").is_none());
    }
}
