use super::{
    Action, App,
    model::{Account, ClientRoot, Dashboard, provider_name},
};
use anyhow::Result;
use chrono::Utc;
use std::{collections::HashMap, path::Path};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};

trait Items {
    fn push(&self, item: &dyn IsMenuItem) -> tray_icon::menu::Result<()>;
}
impl Items for Menu {
    fn push(&self, item: &dyn IsMenuItem) -> tray_icon::menu::Result<()> {
        self.append(item)
    }
}
impl Items for Submenu {
    fn push(&self, item: &dyn IsMenuItem) -> tray_icon::menu::Result<()> {
        self.append(item)
    }
}

#[derive(Default)]
struct Builder {
    actions: HashMap<MenuId, Action>,
}

impl Builder {
    fn label(&self, menu: &dyn Items, text: &str) -> Result<()> {
        menu.push(&MenuItem::new(text, false, None))?;
        Ok(())
    }
    fn separator(&self, menu: &dyn Items) -> Result<()> {
        menu.push(&PredefinedMenuItem::separator())?;
        Ok(())
    }
    fn action(
        &mut self,
        menu: &dyn Items,
        text: &str,
        enabled: bool,
        action: Action,
    ) -> Result<()> {
        let item = MenuItem::new(text, enabled, None);
        self.actions.insert(item.id().clone(), action);
        menu.push(&item)?;
        Ok(())
    }
    fn check(
        &mut self,
        menu: &dyn Items,
        text: &str,
        checked: bool,
        enabled: bool,
        action: Action,
    ) -> Result<()> {
        let item = CheckMenuItem::new(text, enabled, checked, None);
        self.actions.insert(item.id().clone(), action);
        menu.push(&item)?;
        Ok(())
    }

    fn account(&mut self, menu: &Menu, account: &Account, app: &App) -> Result<()> {
        let title = if account.enabled {
            account.title().to_owned()
        } else {
            format!("{} · Disabled", account.title())
        };
        let submenu = Submenu::new(title, true);
        if !account.email.is_empty() {
            self.label(&submenu, &account.email)?;
        }
        let (title, command) = if account.enabled {
            ("Disable Account", "disable")
        } else {
            ("Enable Account", "enable")
        };
        self.action(
            &submenu,
            title,
            !app.mutation_busy,
            Action::mutation(["account", command, &account.id, "--json"]),
        )?;
        self.action(
            &submenu,
            "Sign In Again…",
            app.login.is_none() && !app.mutation_busy,
            Action::Login(account.provider.clone(), account.id.clone()),
        )?;
        self.action(
            &submenu,
            "Rename / Set Alias…",
            !app.mutation_busy,
            Action::Rename(account.clone()),
        )?;
        if !account.alias.is_empty() {
            self.action(
                &submenu,
                "Use Email as Name",
                !app.mutation_busy,
                Action::mutation(["account", "rename", &account.id, "--json"]),
            )?;
        }
        self.action(
            &submenu,
            "Remove from Pool…",
            !app.mutation_busy,
            Action::Remove(account.clone()),
        )?;
        menu.append(&submenu)?;
        let now = Utc::now();
        let rows = account.quota_rows(now);
        if rows.is_empty() {
            self.label(menu, "    Usage unavailable")?;
        }
        for row in rows {
            self.label(menu, &format!("    {row}"))?;
        }
        if account.stale(now) {
            self.label(menu, "    Last snapshot is stale")?;
        }
        Ok(())
    }

    fn configure(&mut self, menu: &Menu, dashboard: Option<&Dashboard>, app: &App) -> Result<()> {
        let config = Submenu::new("Configure", true);
        if let Some(dashboard) = dashboard {
            for provider in ["codex", "claude"] {
                let roots = dashboard
                    .client_roots
                    .iter()
                    .filter(|root| root.provider == provider)
                    .collect::<Vec<_>>();
                let label = format!("Configure {}", provider_name(provider));
                if roots.len() == 1 {
                    self.action(
                        &config,
                        &label,
                        !app.mutation_busy,
                        Action::Integrate(roots[0].clone(), false),
                    )?;
                } else if roots.len() > 1 {
                    let submenu = Submenu::new(label, true);
                    for root in roots {
                        self.action(
                            &submenu,
                            &root.root,
                            !app.mutation_busy,
                            Action::Integrate(root.clone(), false),
                        )?;
                    }
                    config.append(&submenu)?;
                }
            }
        }
        self.action(
            &config,
            "Claude Desktop Setup…",
            true,
            Action::Docs("https://noria.ashray.xyz/docs/native-clients/#what-about-claude-desktop"),
        )?;
        self.action(
            &config,
            "Choose a Custom Client Folder…",
            !app.mutation_busy,
            Action::CustomIntegration,
        )?;
        if let Some(dashboard) = dashboard {
            for integration in &dashboard.integrations {
                let root = ClientRoot {
                    provider: integration.provider.clone(),
                    root: Path::new(&integration.path)
                        .parent()
                        .unwrap_or(Path::new("."))
                        .display()
                        .to_string(),
                };
                self.action(
                    &config,
                    &format!("Restore Direct Settings: {}…", integration.path),
                    !app.mutation_busy,
                    Action::Integrate(root, true),
                )?;
            }
        }
        self.separator(&config)?;
        for provider in ["codex", "claude"] {
            self.action(
                &config,
                &format!(
                    "Add {} Account…",
                    if provider == "codex" {
                        "Codex"
                    } else {
                        "Claude"
                    }
                ),
                app.login.is_none(),
                Action::Login(provider.into(), String::new()),
            )?;
        }
        if app.login.is_some() {
            self.label(&config, "Finish sign-in in your browser…")?;
            self.action(&config, "Cancel Sign-In", true, Action::CancelLogin)?;
        }
        self.action(&config, "Health Check", true, Action::Health)?;
        self.separator(&config)?;
        let preferences = dashboard.map(|d| d.preferences.clone()).unwrap_or_default();
        for (label, key, value) in [
            (
                "Launch Tray at Login",
                "tray-at-login",
                preferences.tray_at_login,
            ),
            (
                "Start Noria at Login",
                "start-at-login",
                preferences.start_at_login,
            ),
            (
                "Automatically Update",
                "auto-update",
                preferences.auto_update,
            ),
        ] {
            self.check(
                &config,
                label,
                value,
                !app.mutation_busy && dashboard.is_some(),
                Action::mutation([
                    "preferences",
                    key,
                    if value { "off" } else { "on" },
                    "--json",
                ]),
            )?;
        }
        self.action(
            &config,
            if app.update_busy {
                "Checking for Updates…"
            } else {
                "Check for Updates…"
            },
            !app.update_busy,
            Action::CheckUpdate(false),
        )?;
        self.action(
            &config,
            "Open Documentation",
            true,
            Action::Docs("https://noria.ashray.xyz/docs/quickstart/"),
        )?;
        self.separator(&config)?;
        let version = dashboard
            .map(|d| d.version.as_str())
            .filter(|version| !version.is_empty())
            .unwrap_or(super::VERSION);
        self.label(&config, &format!("Noria {version}"))?;
        menu.append(&config)?;
        Ok(())
    }
}

pub fn build(app: &App) -> Result<(Menu, HashMap<MenuId, Action>)> {
    let mut builder = Builder::default();
    let menu = Menu::new();
    let dashboard = app.dashboard.as_ref();
    let running = dashboard.is_some_and(|d| d.running);
    builder.label(
        &menu,
        if dashboard.is_none() {
            "Loading Noria…"
        } else if running {
            "Noria is Running"
        } else {
            "Noria is Stopped"
        },
    )?;
    if let Some(error) = &app.status_error {
        builder.label(&menu, error)?;
    }
    builder.action(
        &menu,
        if running { "Stop Noria" } else { "Start Noria" },
        !app.mutation_busy && dashboard.is_some(),
        Action::mutation([if running { "stop" } else { "start" }, "--json"]),
    )?;
    if app.mutation_busy {
        builder.label(&menu, "Applying change…")?;
    }
    if let Some(dashboard) = dashboard {
        for provider in ["codex", "claude"] {
            let accounts = dashboard
                .accounts
                .iter()
                .filter(|account| account.provider == provider)
                .collect::<Vec<_>>();
            if accounts.is_empty() {
                continue;
            }
            builder.separator(&menu)?;
            builder.label(
                &menu,
                if provider == "codex" {
                    "CODEX ACCOUNTS"
                } else {
                    "CLAUDE ACCOUNTS"
                },
            )?;
            for account in accounts {
                builder.account(&menu, account, app)?;
            }
        }
    }
    builder.separator(&menu)?;
    builder.action(
        &menu,
        if app.quota_busy {
            "Refreshing Usage…"
        } else {
            "Refresh Usage"
        },
        !app.quota_busy,
        Action::RefreshQuota,
    )?;
    if app.quota_error {
        builder.label(&menu, "Some accounts need attention · Health Check")?;
    }
    if app.status_error.is_some() {
        builder.action(
            &menu,
            "Refresh Status",
            !app.status_busy,
            Action::RefreshStatus,
        )?;
    }
    builder.separator(&menu)?;
    builder.configure(&menu, dashboard, app)?;
    builder.separator(&menu)?;
    builder.action(
        &menu,
        "Quit Tray App (Keep Noria Running)",
        true,
        Action::Quit,
    )?;
    Ok((menu, builder.actions))
}

pub fn icon(running: bool) -> tray_icon::Icon {
    let mut rgba = vec![0_u8; 48 * 48 * 4];
    let background = if running {
        [24, 33, 43, 255]
    } else {
        [115, 122, 130, 255]
    };
    for y in 0_i32..48 {
        for x in 0_i32..48 {
            let position = ((y * 48 + x) * 4) as usize;
            if (x - 24).pow(2) + (y - 24).pow(2) <= 23 * 23 {
                rgba[position..position + 4].copy_from_slice(&background);
            }
            if (13..=35).contains(&y)
                && ((13..=18).contains(&x)
                    || (30..=35).contains(&x)
                    || ((18..=30).contains(&x) && (x - (18 + (y - 13) * 12 / 22)).abs() <= 3))
            {
                rgba[position..position + 4].copy_from_slice(&[255; 4]);
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, 48, 48).expect("valid fixed-size tray icon")
}
