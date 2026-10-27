---
title: Changelog
description: Highlights from Noria releases.
order: 11
category: Reference
---

## 0.4.2 · October 28, 2026

- Increased the bounded HTTP and WebSocket native request body limit from 16 MiB to 256 MiB to support image-heavy conversation history in Codex and Claude sessions.
- Added early rejection for requests whose declared `Content-Length` exceeds the cap, and updated HTTP 413 responses to report the configured 256 MiB limit.

## 0.4.1 · October 23, 2026

- Windows and Linux installers now enable tray startup at login and launch the tray immediately in graphical sessions. Headless Linux installations remain usable without a desktop.
- Tray apps automatically reopen after successful Windows/Linux upgrades. Updates started outside a Linux graphical session retain desktop autostart for the next login.
- Added checksum-verified, on-demand installation of a missing tray companion to support older CLI-only installations. Improved Linux D-Bus session detection and diagnostics for GNOME desktops without an active AppIndicator host.

## 0.4.0 · October 23, 2026

- Added lightweight Windows and Linux tray apps built with `gogpu/systray`, offering the same account management, quota/reset displays, Start/Stop, health checks, client configuration, login preferences, and update controls as the macOS menu bar. The macOS app remains native Swift.
- Added per-user desktop autostart for Windows and mainstream Linux desktop sessions, platform-native confirmation and folder dialogs, and tray startup/quit CLI commands. Quitting the tray never stops routing.
- Updated verified installers, paired CLI/tray upgrades, and release assets for Windows and Linux on x64 and ARM64, with safe replacement of running Windows tray executables.
- Clarified that smart Claude Code routing is experimental while OAuth refresh and proxy compatibility are refined. No Claude proxy or authentication implementation was changed in this release.

## 0.3.6 · October 20, 2026

- Fixed Claude Code requests failing after an account's OAuth access token expires. Noria now refreshes the isolated account credentials before proxying requests, rotates the refresh token safely under the native profile lock, and preserves existing credential settings without an extra sign-in or model prompt.
- Made unrecoverable Claude authentication failures report a clear **sign in again** message instead of looking like exhausted usage. Added concurrency, invalid-token, and sensitive-data regression tests.

## 0.3.5 · October 20, 2026

- Corrected Codex subscription-limit classification so old/resumed sessions automatically fail over to a different available account when an exhausted account returns a generic usage-limit response, without confusing ordinary per-minute throttling with subscription exhaustion.
- Fixed Claude Code's **Usage unavailable** when its installed native CLI reports `rate_limits: null` for an authenticated account. Noria still queries the native binary first, then uses that account's isolated OAuth credential to read the provider's read-only usage endpoint directly from Go (no Node, SDK, or model prompt). Quota percentages and actual reset timestamps are preserved, and unrelated breakdown/billing metadata is ignored.

## 0.3.4 · October 20, 2026

- Fixed automatic update checks when GitHub's anonymous release API is rate-limited (HTTP 403). Noria now discovers stable releases through GitHub's public latest-release redirect, with strict URL validation and its existing checksum-verified asset downloads.

## 0.3.3 · October 20, 2026

- Made macOS **Automatically Update** register an independent hourly launchd job, so verified updates continue when Noria's menu bar or router is not running. Startup repairs missing registrations, and disabling the setting removes the job.
- Hardened menu bar restarts after updates: wait for the previous process to exit before reopening, and show an actionable repair option if the updater registration fails.
- Added isolated launchd registration tests, documentation, and installation-time updater setup.

## 0.3.2 · October 20, 2026

- Reorganized the macOS menu bar: the main menu shows populated providers, their reported quota windows, Start/Stop, and Refresh. Account setup, client settings, health, startup preferences, upgrades, and documentation are under **Configure**.
- Renamed client setup to **Configure Codex** and **Configure Claude Code**. Empty provider sections and unavailable five-hour quota lines no longer clutter the menu.
- Rebuilt the Noria website, docs, and README around local smart routing, account switching, and how to use the application. Added a lightweight interactive routing example and a clearer algorithm guide.

## 0.3.1 · October 20, 2026

- Fixed **Configure Codex** crashing when an existing `config.toml` includes nested arrays of tables such as `[[skills.config]]`.
- Preserved the original `model_provider` line in its original location when restoring direct settings.

## 0.3.0 · October 20, 2026

- Switched entirely to native macOS, Linux, and Windows installers and GitHub Releases.
- Added `noria upgrade` for the CLI and native menu bar updates.
- Made account setup email-first with optional aliases; removed the previous named-account registration path.

## 0.2.0 · October 20, 2026

- Added a native macOS menu bar for usage, reset times, account management, and Start/Stop.
- Added nonblocking background service management on macOS, Linux, and Windows.
- Made Codex and Claude Code routing persistent and reversible through their regular configuration files.
- Added automatic updates, health checks, and browser-based account sign-in.

## 0.1.1 · October 20, 2026

- Switched Claude usage readings to the installed native `claude` executable.

## 0.1.0 · October 20, 2026

- Initial local proxy with Codex/Claude account pools, quota-aware selection, sticky sessions, and safe failover.

See all tagged releases and assets on [GitHub](https://github.com/amxv/noria/releases).
