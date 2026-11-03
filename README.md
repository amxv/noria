# Noria

**Stop switching accounts. Start using more of your limits.**

Noria is a **smart local proxy for Codex and Claude Code**. Connect the accounts you already use, point your coding clients at Noria once, and keep coding normally. Noria chooses where new sessions go, keeps them on the same account for prompt caching, and switches to another usable account when a limit gets in the way.

No more logging out of one account just to log into another. No more manually tracking when five-hour and weekly allowances reset.

**[Website](https://noria.ashray.xyz)** · **[Get started](https://noria.ashray.xyz/docs/quickstart)** · **[How routing works](https://noria.ashray.xyz/docs/architecture)** · **[Releases](https://github.com/amxv/noria/releases)**

## Why use Noria?

Your accounts each have their own usage limits, their own reset times, and their own prompt caches. Picking an account is more complicated than choosing the one with the biggest number remaining.

Noria handles three things:

1. **Smarter placement.** New sessions consider the remaining five-hour and weekly allowances, when each resets, and how much work is already assigned. A little quota about to reset can be worth using before a larger allowance that lasts for days.
2. **Sticky conversations.** A conversation stays on its chosen account as long as that account can serve it. This preserves account-level prompt-cache reuse instead of bouncing between accounts on every request.
3. **Automatic failover.** If an applicable limit blocks the next request, Noria looks for another eligible account of the *same provider* and safely retries the pending request. Then it stays on that account.

```text
                        Noria (localhost)
Codex / Claude Code ─────────┬───────────────→ Your account A
                            ├───────────────→ Your account B
                            └───────────────→ Your account C

                   One local route. Multiple accounts.
```

Noria optimizes the allowances you already have; it doesn't generate extra quota or guarantee work can continue after every account is exhausted.

*Claude Code smart routing is experimental. OAuth token refresh and proxy compatibility are still being refined, so you may encounter bugs.*

## Install

**macOS and Linux**

```sh
curl -fsSL https://noria.ashray.xyz/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://noria.ashray.xyz/install.ps1 | iex
```

The installer includes a native Swift menu bar app on macOS and a Rust tray app on Windows and Linux for account management, quota usage, and Start/Stop controls. The tray opens on installation, starts at login by default, and reopens on upgrades. Most mainstream Linux desktops, including standard Ubuntu, KDE Plasma, and Linux Mint Cinnamon, have tray support already. Some GNOME installations need their AppIndicator extension enabled.

## Set up once

```sh
# Add accounts (each opens the provider's browser sign-in)
noria account add codex
noria account add codex
noria account add claude

# Start the background proxy
noria start

# Route your existing clients through Noria
noria integrate codex --yes
noria integrate claude --yes
```

The integration changes the relevant client configuration, not your existing conversations or sign-in. **Restart any clients you already have open.** Then launch `codex` or `claude` exactly as before—no wrapper command required.

If your client uses a custom configuration folder, specify it with `--client-home`. See the [setup guide](https://noria.ashray.xyz/docs/quickstart) for desktop apps and alternative paths.

## Day to day

```sh
noria status            # Account usage and reset times
noria quota refresh     # Fetch a fresh usage snapshot
noria health            # Check local routing configuration
noria stop              # Return managed clients to direct provider routing
noria start             # Start routing again
noria upgrade           # Upgrade the CLI and matching desktop app
noria tray open         # Open the Windows/Linux tray (macOS: noria menubar open)
```

Accounts are displayed by **email address by default**. You can optionally set aliases, enable or disable individual accounts, and sign in again from the CLI or desktop tray/menu bar.

Everything Noria manages lives locally. Its gateway runs on loopback, and each upstream account has separate credentials. Model requests still go to their respective provider—Noria is a router, not a local model.

## Important boundaries

Noria doesn't switch providers or models, purchase usage, move prompt caches between accounts, or splice an already-delivered partial response. Failover retries a request only when safe. Live subscription behavior depends on each provider's supported client interfaces and terms.

The [architecture guide](https://noria.ashray.xyz/docs/architecture) explains the algorithm, and [security and limitations](https://noria.ashray.xyz/docs/security-and-limitations) covers the boundaries in more detail.

## Develop

Noria's CLI and router are written in Rust. Its macOS menu bar remains native Swift; Windows and Linux use `tray-icon` in a separate Rust executable. Linux uses its native StatusNotifierItem D-Bus protocol through `ksni`, with no GTK or AppIndicator library dependency. Desktop event loops sleep between menu actions, file notifications, and scheduled refreshes. All desktop controls wrap the CLI, which owns accounts, routing, settings, and updates. The website uses Astro and ZueDocs.

Use Rust 1.90 or newer and Python 3.11 or newer. The previous Go implementation remains in `cmd/` and `internal/` as a behavior reference during migration; release builds use Cargo.

```sh
make check
make test-go-parity
make build
make build-tray # Windows/Linux; macOS uses apps/menubar/build.sh
make docs-install
make docs-check
```

See [Contributing](CONTRIBUTORS.md). Licensed under [Apache-2.0](LICENSE).
