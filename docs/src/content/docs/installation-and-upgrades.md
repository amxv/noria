---
title: Installation and upgrades
description: Install Noria on your platform, upgrade it, and troubleshoot installation problems.
order: 8
category: Reference
---

## Install

**macOS and Linux**

```sh
curl -fsSL https://noria.ashray.xyz/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://noria.ashray.xyz/install.ps1 | iex
```

macOS includes the native Swift menu bar app. Windows and Linux include a Rust tray app with the same account and router controls. The installer opens the desktop UI when a graphical session is available and registers it to start on subsequent logins by default. The background router remains independent of the UI. Set `NORIA_NO_LAUNCH=1` during automated installations to skip GUI startup and startup registration.

On Linux, the tray uses the desktop's StatusNotifierItem/AppIndicator support over the user D-Bus session, without depending on GTK or the libappindicator library. **Ubuntu Desktop, KDE Plasma, and Linux Mint Cinnamon generally work out of the box.** Xfce requires its status notifier panel plugin; Fedora Workstation and other upstream GNOME installations may need to enable the [AppIndicator and KStatusNotifierItem Support extension](https://extensions.gnome.org/extension/615/appindicator-support/). Noria reports when a tray host is unavailable, and its tray can register when a host becomes active. Dialog actions use `zenity` or `kdialog`; headless servers can use the CLI without a tray. Linux releases use static musl binaries, so they do not require a particular glibc version. A graphical desktop is required only for the tray; the CLI also runs on headless systems.

You can also find binaries and macOS app archives on [GitHub Releases](https://github.com/amxv/noria/releases).

## Upgrade

```sh
noria upgrade --check
noria upgrade
```

On any desktop, choose **Configure → Check for Updates…** in Noria's tray/menu bar. You can turn on **Automatically Update** there or run `noria preferences auto-update on`.

With **Automatically Update** enabled, macOS schedules hourly checks using a per-user launchd job. These checks continue after quitting the menu bar or stopping routing, and after logging in again. Failed network checks retry at the next interval. Noria installs a verified release only when it is safe to stop and restart the router; otherwise it retries later. The update log is in `~/.noria/logs/updates.log`.

Turning off the setting unregisters the automatic update job. If the menu bar reports that automatic updates need repair, select **Configure → Repair Automatic Updates…** or run `noria preferences auto-update on` again.

On Linux and Windows, automatic checks also run from the tray while it is open, or from the router while it is running. Noria verifies an update before installing it and defers replacement while requests are active. The matching CLI and desktop application are updated as a unit, including Windows' detached helper for replacing in-use executables. Successful upgrades automatically open or reopen the tray in desktop sessions. Headless Linux upgrades leave the tray to launch at the next desktop login.

## Problems installing?

If `noria` isn't found after installation, open a new terminal and ensure your install directory is on `PATH`. On macOS/Linux, the default is `~/.local/bin`.

If Noria is already installed, use `noria upgrade` rather than rerunning the installation script.

For issues with a macOS downloaded app, install through the documented command rather than copying the app bundle manually. Noria's macOS app uses an ad-hoc signature, not Developer ID notarization; security policies on managed devices may differ.

For other errors, see [Troubleshooting](/docs/troubleshooting).

## Release information

Noria releases are tagged in GitHub, with macOS, Linux, and Windows binaries for supported architectures. The installer verifies release checksums. The code and current releases are available at [amxv/noria](https://github.com/amxv/noria).

The website uses ZueDocs and deploys on Git changes to `docs/` or the install scripts; application-code-only changes do not rebuild it.
