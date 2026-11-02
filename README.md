# agentspeedrun

Speedrun timing for agentic coding projects. Every project is a **run** that
starts with its first prompt and is timed against a **category** of
**milestones** (splits):

| Category          | Finish line |
|-------------------|-------------|
| `any-percent`     | Concept demo: the happy path works end-to-end; internals may be stubbed. |
| `hundred-percent` | Complete mini-product: real core features, tested, deployed, usable by a stranger. |
| `first-revenue`   | First real payment, taken soundly: deployed, secured, legal pages, live payments, tax set up. |

Milestones come from the category, from you (`speedrun add`), or from the
agent, which proposes project-specific milestones after the first prompt.
Every prompt is stored with its timing (after secret redaction).

## How it works

```
Claude Code / opencode / pi / VS Code / Zed      thin adapters (hooks, MCP)
                   │  speedrun CLI: spools to disk if the daemon is down
                   ▼
          speedrund (local daemon)                append-only, hash-chained
          SQLite event log → run projections      event log; timing derived
```

- **Local-first.** One daemon per machine owns the data, listening on loopback with a bearer token. No network needed.
- **Event-sourced.** Runs are folds over an append-only log; the database rejects updates and deletes.
- **Signed and chained.** Each event is signed by its device's key and hashes the one before; `speedrun verify` checks both with public keys only. Whoever holds a device's private key can still rewrite and re-sign that device's local runs; a published run cannot change ([docs/COMMONS.md](docs/COMMONS.md)).
- **Idempotent and offline-safe.** Every write carries a client event id; writes made while the daemon is down are spooled and replayed.
- **Trust levels.** A milestone counts only at its category's required trust: `self_declared` < `agent_claimed` < `evidence_verified`. Only daemon verifiers grant `evidence_verified`: git commits in the project's history, live public HTTPS pages (including terms and privacy content), passing GitHub Actions runs, and real live-mode Stripe payments.
- **Several clocks.** Real time, timed (minus pauses), agent time (turns, unioned across sessions) and human time (gaps between turns, capped at 15 min each).

Design details: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Building a widget, picture-in-picture or phone view? See [docs/LIVE_DISPLAY.md](docs/LIVE_DISPLAY.md). Runs are private by default; the proposal for publishing them as a commons — device signing, disclosure tiers, external-clock brackets — is [docs/COMMONS.md](docs/COMMONS.md).

## Quick start

Prebuilt binaries for Linux (x86_64, arm64; static) and macOS (Apple silicon, Intel). Windows is not supported; use WSL.

```bash
curl -fsSL https://raw.githubusercontent.com/mesh-inc/agentspeedrun/main/install.sh | sh
```

While the repository is private, go through an authenticated GitHub CLI instead:

```bash
gh api repos/mesh-inc/agentspeedrun/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

The installer checks the release's `SHA256SUMS` and puts `speedrun` and `speedrund` in `~/.local/bin` (`SPEEDRUN_INSTALL_DIR` to change, `SPEEDRUN_VERSION=v0.1.0` to pin). Or build from source:

```bash
cargo install --path crates/speedrund && cargo install --path crates/speedrun-cli
```

In the repo you want to time:

```bash
speedrun init --category any-percent
```

Then connect your agent and start prompting. The daemon starts automatically on first use.

| Host | Install |
|------|---------|
| Claude Code | `/plugin marketplace add mesh-inc/agentspeedrun` ([details](plugins/claude-code)) |
| opencode | `speedrun setup opencode` ([details](plugins/opencode)) |
| pi | `speedrun setup pi` ([details](plugins/pi)) |
| oh-my-pi | `speedrun setup omp` ([details](plugins/pi)) |
| Copilot agent (VS Code, Copilot CLI) | `speedrun setup copilot --project` ([details](plugins/copilot)) |
| VS Code (timer, splits view, MCP tools) | `code --install-extension agentspeedrun-<version>.vsix` from the release ([details](plugins/vscode)) |
| Zed (MCP tools) | dev extension in [plugins/zed](plugins/zed) |

```bash
speedrun status                              # timer and splits
speedrun split happy-path-demo --commit HEAD # claim a split with evidence
speedrun split deployed --url https://myapp.com
speedrun split tests-green --ci https://github.com/me/app/actions/runs/123
speedrun secret set stripe                   # restricted rk_live_ key, read-only
speedrun split first-payment --stripe pi_3Q...
speedrun accept <id>                         # accept an agent-proposed milestone
speedrun pause / speedrun resume
speedrun verify                              # check the hash chain
speedrun watch                               # live view of all active runs
speedrun display                             # read-only token + URLs for widgets
```

## Layout

| Path | What |
|------|------|
| `crates/speedrun-core` | Event types, hash chain, categories, projections and timing, redaction, API types |
| `crates/speedrund` | Daemon: SQLite store, rules, verifiers, HTTP API |
| `crates/speedrun-cli` | `speedrun` CLI, host hook adapters, MCP server, offline spool |
| `categories/` | Built-in category rulesets (versioned JSON) |
| `schema/protocol.schema.json` | JSON Schema of the protocol, including `LiveSnapshot` (CI checks it is current) |
| `schema/examples/` | Generated example payloads for display and design work |
| `plugins/` | Host adapters: Claude Code, opencode, pi/oh-my-pi, Copilot hooks, VS Code, Zed |

## Status

Scaffold. Working: daemon, CLI, Claude Code plugin, MCP tools, git, URL, legal-page, GitHub Actions and Stripe verifiers,
offline spool, live-display stream with read-only tokens and personal-best comparison. Adapters: Claude Code, opencode, pi, oh-my-pi, Copilot agent (VS Code and
CLI), VS Code extension, Zed extension.
Not yet: encryption of prompts at rest, prompt
capture for Zed's native agent (no API), cloud sync and leaderboards.
