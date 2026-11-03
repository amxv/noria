# Noria development contract

Work on the user-selected machine and worktree. Do not delegate when the user has prohibited delegation. Never run test prompts or authentication against the developer's actual subscription profiles.

## Architecture

Rust owns account identity, hard session affinity, quota snapshots, persistent integration backups, lifecycle, preferences, and updates. Native Swift is a presentation wrapper around CLI commands and JSON, not a second scheduler or settings store. The Windows/Linux Rust tray follows the same CLI contract. Keep macOS native Swift. The original Go sources remain a behavior reference during the migration and are not used for release binaries.

Public lifecycle is `start`, `stop`, `restart`. There is no public `serve` command; internal daemon commands exist for OS supervision. Stop restores managed direct-client configuration without copying pool credentials into ordinary native profiles. Reopen already-running clients when settings are cached.

Integration is an explicit, previewable, reversible user-authorized operation. Tests must use temporary CODEX_HOME, CLAUDE_CONFIG_DIR, NORIA_HOME and HOME. Preserve unrelated edits and comments. Refuse to overwrite external routing changes.

Accounts display email by default; aliases are optional and never replace immutable routing IDs. Native logins retain provider-owned OAuth storage/callbacks. Do not log auth URLs, secrets, tokens, or model request content.

## Checks

Run `make check`, `make test-go-parity`, native persistent-config fixtures, and cross-platform service tests. Build Swift for arm64 and x86_64, verify signatures, and exercise CLI/menu JSON. Native CI covers Linux, Windows, and macOS on both amd64 and arm64. Use `make docs-check` for routine Astro validation; the deploy workflow builds the site in CI. The tray must use sleeping native event loops and filesystem notifications rather than short-interval polling; Linux uses tray-icon's ksni backend without GTK.

Code files should stay below 1000 lines. Add tests for lifecycle idempotence, config roundtrips/conflicts, stable aliases, atomic reservations, safe early failover, deferred updates, and release checksum/archive validation.

## Distribution

Use the Zodex-style checksum-verified CLI download/extract path with an ad-hoc signed macOS app. Never disable Gatekeeper, clear quarantine, or claim Developer ID notarization. Browser downloads and managed policies may differ. macOS CLI and Noria.app are sibling artifacts.

Update VERSION, Cargo.toml/Cargo.lock, and the manual changelog, verify CI, then push the release tag. Confirm six binaries, two macOS Swift archives, four Windows/Linux tray archives, dependency license texts, checksums, native installer smoke tests, and docs deployment. Native builds use `scripts/build-release.py`; full releases require the six native CI runners. Never publish from a concurrently changing checkout or discard someone else's worktree changes.

The docs site is a separate Bun/Astro workspace. Its production deployment is managed by `.github/workflows/docs-deploy.yml` for changes to `docs/**` or the two root installer scripts; unrelated Rust/Swift code pushes create **no Vercel deployment**. `docs/vercel.json` disables Git-triggered deployments and has no `ignoreCommand`. Keep the GitHub connection and Vercel Root Directory `docs`. CI builds static output and deploys via `--prebuilt` using the project-scoped `VERCEL_TOKEN` GitHub Actions secret and `VERCEL_ORG_ID` / `VERCEL_PROJECT_ID` Actions variables. Keep this path filter tested with `cd docs && bun run test:deployment`; don't introduce duplicate deployment triggers or account-wide tokens into CI.
