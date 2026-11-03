# Contributing to Noria

Noria is a local Rust gateway for native Codex and Claude Code accounts, released as standalone binaries with checksum-verified install scripts. Start with the [development guide](https://noria.ashray.xyz/docs/development) and [routing contract](https://noria.ashray.xyz/docs/architecture).

## Develop and validate

Install Rust 1.90 or newer with rustfmt and Clippy, plus Python 3.11 or newer for native release checks. Use the Go version in `go.mod` only for the retained reference tests. Bun is required only for the documentation website; Xcode command-line tools build the native Swift macOS app. End users need none of these build tools.

Linux's Rust tray uses `tray-icon` with `ksni` and the desktop's D-Bus session, so building it does not require GTK, libappindicator, or a webview. A StatusNotifierItem panel host is required to display it; `zenity` or `kdialog` provides optional input dialogs. Windows uses the native Win32 message loop and menus.

```sh
make check
make test-go-parity
make build
make build-tray # Windows/Linux
make docs-install
make docs-check
make docs-build
```

Run Astro docs checks serially. Application tests must always isolate `HOME`, `NORIA_HOME`, `CODEX_HOME`, and `CLAUDE_CONFIG_DIR`, using fake credentials and upstreams. Never touch real subscription accounts, provider configuration or conversation history in automated tests.

Preserve hard session affinity, independently timed quota windows, atomic admission and reservations, native protocol fidelity, and safe failover before output is committed. Never move healthy sessions because a score changes. Don't include tokens, auth files, transcripts, or the output of `noria auth-token` in logs.

## Native releases

Update `VERSION`, `Cargo.toml`'s package version, and the manually curated `docs/src/content/docs/changelog.md` together; refresh `Cargo.lock`. Run tests and verify the macOS arm64/x86_64 menu bar builds and ad-hoc signatures. Check both native installers, all six raw binaries, and all six desktop archives.

`make release-native` builds and validates the current host's binary and archive. The full release workflow builds Linux, macOS, and Windows on both amd64 and arm64 native runners; `make build-all` explains this requirement instead of silently omitting unsupported targets. Linux releases use static musl builds and Windows releases link the MSVC runtime statically, preserving standalone installation. Linux native release builds require `musl-tools` and the matching Rust musl target. Every archive includes the full license text generated from locked Cargo dependencies, and release validation checks every archive and SHA-256 checksum before publication.

From a clean `main` branch, push the changes, then use `make release-tag VERSION=x.y.z`. GitHub Actions builds binaries and apps, writes SHA-256 checksums, publishes GitHub Releases, and tests fresh installs on macOS, Linux, and Windows. Noria does not publish a JS or npm package.

macOS installation follows Zodex's checksum-verified curl/tar pattern for its ad-hoc-signed `.app`. We never disable Gatekeeper or remove quarantine from downloaded files. The docs site deploys through the existing GitHub Actions workflow to Vercel from the `docs/` root.

See the [installation and upgrade reference](https://noria.ashray.xyz/docs/installation-and-upgrades/) and repository release skill. Contributions are licensed under Apache-2.0.
