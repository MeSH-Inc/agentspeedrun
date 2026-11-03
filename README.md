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
Prompt text is redacted on a best-effort basis and encrypted locally. Redaction
does not guarantee that every secret is removed. Use `speedrun preferences
capture metadata-only` to record timing without prompt text or text commitments.

## How it works

```
Claude Code / opencode / pi / VS Code / Zed      thin adapters (hooks, MCP)
                   │  speedrun CLI: spools to disk if the daemon is down
                   ▼
          speedrund (local daemon)                append-only, hash-chained
          SQLite event log → run projections      event log; timing derived
```

- **Local-first.** One daemon per machine owns the data, listening on loopback with a bearer token. Core timing works offline; some evidence checks and publication use the network.
- **Event-sourced.** Runs are folds over an append-only log; the database rejects updates and deletes.
- **Signed and chained.** Each event is signed by its device's key and hashes the one before; `speedrun verify` checks both with public keys only. Whoever holds a device's private key can still rewrite and re-sign that device's local runs; a published run cannot change ([docs/COMMONS.md](docs/COMMONS.md)).
- **Idempotent and offline-safe.** Every write carries a client event id; writes made while the daemon is down are spooled and replayed.
- **Trust levels.** A milestone counts only at its category's required trust: `self_declared` < `agent_claimed` < `evidence_verified`. Only daemon verifiers grant `evidence_verified`: git commits in the project's history, live public HTTPS pages (including terms and privacy content), passing GitHub Actions runs, and real live-mode Stripe payments.
- **Several clocks.** Real time, timed (minus pauses), agent time (turns, unioned across sessions) and human time (gaps between turns, capped at 15 min each).

Design details: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Building a widget, picture-in-picture or phone view? See [docs/LIVE_DISPLAY.md](docs/LIVE_DISPLAY.md). Runs are local by default. Publication to a configured commons is explicit; [docs/COMMONS.md](docs/COMMONS.md) describes the implemented publication protocol and future proposals.

## Quick start

Prebuilt binaries for Linux (x86_64, arm64; static) and macOS (Apple silicon, Intel). Windows is not supported; use WSL.

```bash
curl -fsSL https://raw.githubusercontent.com/MeSH-Inc/agentspeedrun/main/install.sh | sh
```

The installer checks the release's `SHA256SUMS`, and with an authenticated GitHub CLI also the archive's build attestation (`gh attestation verify`). It puts `speedrun` and `speedrund` in `~/.local/bin` (`SPEEDRUN_INSTALL_DIR` to change, `SPEEDRUN_VERSION=v0.2.0` to pin) and restarts a running daemon on the new version. Or build from source, in a clone:

```bash
cargo install --locked --path crates/speedrund && cargo install --locked --path crates/speedrun-cli
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
speedrun backup ~/speedrun-backup.sqlite     # copy the whole log, daemon running
```

Local test observations retain the number of command arguments but omit their
values; failure messages omit command output.

Use `speedrun preferences show` to inspect capture settings. `speedrun prompts
show <event-id>` reads retained text, and `speedrun prompts erase <event-id>`
removes it while retaining the signed event and any disclosure commitment.
`speedrun preferences retention <days>` sets an age limit; use `forever` to
disable it. The daemon applies retention at startup and approximately hourly.
Tier 2 publication is unavailable for a run with metadata-only prompts.
Prompt ciphertext uses a private `content.key` file beside the database. This
protects a database copy without that key; it does not protect against theft
of the whole data directory. Erasure does not promise physical deletion from
SQLite journals, filesystem snapshots, backups, or storage devices.
Pending prompt text is also encrypted in the durable outbox under a separate
`spool/outbox.key`. Existing legacy JSONL files remain plaintext until an
explicit `speedrun flush` migrates them; unreadable records are encrypted and
retained as rejected diagnostics while later records are replayed.

### Backups

Everything lives in the data directory: `$SPEEDRUN_HOME`, else
`$XDG_DATA_HOME/agentspeedrun`, else `~/.local/share/agentspeedrun`.
`speedrun backup <file>` copies the event log there while the daemon runs:
one consistent snapshot (SQLite `VACUUM INTO`), written to a new file that only
you can read. Prompt text in new snapshots is encrypted, but other local data
can still be sensitive. Every run in the copy verifies
with public keys alone. The secret keys are not in it: `device.key` (signs as
this device), `prompt.key`, `content.key`, and the per-run keys in `disclosure/` (needed to
export a run's prompt commitments). Keep those with your other secrets.

Inspect requests that the daemon permanently rejected with `speedrun outbox rejected`.
Use `--json` for machine-readable output, `--limit` to set the page size, and
`--after <id>` to continue from the returned cursor. Pages browse the records
currently retained in enqueue order; they are not a snapshot or a watch cursor.
A pending older request may be rejected after a later page was read, so restart
from the beginning to see all current rejections. The output contains the
HTTP status and rejection time when recorded, but never request bodies or daemon
error messages. Older records can have an unknown reason; unimported legacy
records are reported as a count. Rejected records are retained within a bounded
size and may be pruned as newer rejections arrive.

This database-only archive is not a complete recovery bundle: it omits keys
and pending writes. Use the recovery commands below to preserve those together.

For a recovery bundle that includes the local device identity and pending
outbox, stop the daemon, then run:

```bash
speedrun daemon stop
speedrun recovery create ~/speedrun-recovery
speedrun recovery restore ~/speedrun-recovery ~/speedrun-restored
```

The bundle and restored directory are private and contain sensitive keys and
potentially sensitive recorded data. They include the event database, device,
prompt and content keys, disclosure keys, the outbox and its encryption key,
and trusted-project and preference settings. They exclude bearer tokens and
verifier credentials in `secrets/`; configure those again after restoring. The
restore destination must not already exist. Start the restored daemon with
`SPEEDRUN_HOME` set to that directory, and review trusted project paths before
using it on another machine. Bundle checksums detect accidental changes but do
not authenticate the bundle against a malicious replacement. A recovery
snapshot captures the outbox at a point in time: prompts arriving afterwards
are not in that snapshot.

## Layout

| Path | What |
|------|------|
| `crates/speedrun-core` | Event types, hash chain, categories, projections and timing, redaction, API types |
| `crates/speedrund` | Daemon: SQLite store, rules, verifiers, HTTP API |
| `crates/speedrun-cli` | `speedrun` CLI, host hook adapters, MCP server, offline spool |
| `categories/` | Built-in category rulesets (versioned JSON) |
| `schema/protocol.schema.json` | JSON Schema of the protocol, including `LiveSnapshot` (checked by local tests) |
| `schema/examples/` | Generated example payloads for display and design work |
| `plugins/` | Host adapters: Claude Code, opencode, pi/oh-my-pi, Copilot hooks, VS Code, Zed |

## Status

Scaffold. Working: daemon, CLI, Claude Code plugin, MCP tools, git, URL, legal-page, GitHub Actions and Stripe verifiers,
offline spool, live-display stream with read-only tokens and personal-best comparison. Adapters: Claude Code, opencode, pi, oh-my-pi, Copilot agent (VS Code and
CLI), VS Code extension, Zed extension.
Not yet: prompt capture for Zed's native agent (no API), cloud sync and leaderboards.

## Local verification

Run `scripts/verify.sh` from a checkout. It checks Rust formatting, Clippy and
tests; manifest versions; adapter typechecks and tests; generated protocol
artifacts; the Zed extension; and an isolated binary smoke test. Install Rust,
Node.js, Bun, the dependencies in `plugins/opencode`, `plugins/pi` and
`plugins/vscode`, and the Rust `wasm32-wasip2` target first. The VS Code host
integration test (`npm run test:vscode` in `plugins/vscode`) is separate and
requires a working VS Code test environment.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
