---
title: Development
description: Build and test Noria, understand its main packages, and edit the ZueDocs site.
order: 10
category: Reference
---

Noria is a Rust smart proxy with a native Swift macOS menu bar app. Windows and Linux use a separate Rust tray executable that calls the same CLI and JSON interface. The documentation site uses Astro and ZueDocs.

## Build from source

Install Rust 1.90 or newer with rustfmt and Clippy, plus Python 3.11 or newer. macOS app builds also require Xcode command-line tools. Linux's tray uses `tray-icon` with the `ksni` backend: no GTK, libappindicator, webview, or Node runtime is needed to build or run it. The desktop must provide a user D-Bus session and a StatusNotifierItem host. Optional input dialogs use `zenity` or `kdialog`.

```sh
git clone https://github.com/amxv/noria.git
cd noria
make check
make test-go-parity
make build
make build-tray # Windows/Linux only
```

`make release-native` builds this host's release binary and desktop archive, including complete dependency licenses. The macOS menu bar source stays in `apps/menubar/`; it is built and ad-hoc signed with Swift. CI builds all six OS/architecture combinations on native runners, including Windows arm64 and both macOS architectures. `make build-all` explains the native-runner requirement rather than attempting unsupported cross-compilation. Linux releases use static musl binaries; install `musl-tools` and the matching Rust musl target for `make release-native`. Windows release builds link the MSVC runtime statically.

The Go implementation remains under `cmd/` and `internal/` as a behavior reference during migration. `make test-go-parity` runs its isolated fixtures with the race detector using the version in `go.mod`; it does not produce release binaries. Cargo is the production build path, and `Cargo.lock` is checked in.

## Code map

| Directory | Purpose |
| --- | --- |
| `src/core/` | Session affinity, quota windows, placement, and reservations |
| `src/provider/` | Separate native Codex and Claude account handling |
| `src/proxy/` | Local HTTP/SSE and WebSocket routing |
| `src/integration/` | Persistent client configuration and restoration |
| `src/service/` | Background service and login management |
| `src/app/` | CLI commands, status, desktop lifecycle, and upgrades |
| `src/tray/` | Windows/Linux native menus and CLI presentation |
| `src/config.rs`, `src/model.rs` | Persistent configuration and shared JSON models |
| `cmd/`, `internal/` | Retained Go reference implementation and fixtures |
| `apps/menubar/` | Swift user interface for the CLI |
| `docs/` | Astro/ZueDocs website |

## Test safely

All tests must use temporary `HOME`, `NORIA_HOME`, `CODEX_HOME`, and `CLAUDE_CONFIG_DIR` locations, plus fake credentials and test upstreams. On Windows also isolate `USERPROFILE`, `APPDATA`, and `LOCALAPPDATA`. Never run automated tests against your personal subscriptions or modify real client settings.

Preserve three key invariants: healthy sessions remain on their account, different quota windows reset independently, and failed requests are retried only before output has been committed. Keep provider protocols and model IDs unchanged.

## Work on the website

```sh
make docs-install
make docs-dev
make docs-check
```

Keep ZueDocs for the shared documentation layouts and use project-local styles and components for Noria. Run the check and build serially, not simultaneously.

The Vercel project keeps `docs/` as its root. `.github/workflows/docs-deploy.yml` builds and deploys only when `docs/` or the root native install scripts change; application-code-only changes skip the site build. Vercel's Git deployment trigger is disabled to avoid duplicate builds.

For contributions and release details, see [CONTRIBUTORS.md](https://github.com/amxv/noria/blob/main/CONTRIBUTORS.md).
