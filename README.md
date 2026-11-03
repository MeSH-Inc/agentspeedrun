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

## Quick start

This guide describes the 0.3.3 preview. If its assets are not yet available on
the Releases page, build this checkout to use the guided setup and milestone
commands below:

```bash
cargo install --locked --path crates/speedrund
cargo install --locked --path crates/speedrun-cli
```

Ensure `~/.cargo/bin` is on your `PATH`. Published v0.2.0 binaries are also
available for Linux (x86_64 and arm64, static) and macOS (Apple silicon and
Intel); Windows users can use WSL. To install that release, run:

```bash
curl -fsSL https://raw.githubusercontent.com/MeSH-Inc/agentspeedrun/main/install.sh | sh
```

The installer verifies `SHA256SUMS` and, when an authenticated GitHub CLI is
available, the build attestation. It installs to `~/.local/bin` (override with
`SPEEDRUN_INSTALL_DIR`; pin with `SPEEDRUN_VERSION=v0.2.0`). For v0.2.0, use
`speedrun init`, the explicit host setup commands below, and `speedrun status`;
run `speedrun --help` for the commands available in that release.

When the 0.3.3 preview assets are available, install that exact version with:

```bash
curl -fsSL https://raw.githubusercontent.com/MeSH-Inc/agentspeedrun/v0.3.3/install.sh | SPEEDRUN_VERSION=v0.3.3 SPEEDRUN_SKIP_ATTESTATION=1 sh
```

The preview assets are locally built and have no build provenance attestations.
The installer still verifies their published checksums; the command skips only
attestation verification for this preview.

In the repo you want to time:

```bash
speedrun setup
```

Guided setup selects your host, adapter scope, project category, and device-wide
prompt capture preference. It initializes or trusts the current project without
replacing an existing project. Setup does not start the daemon; the read-only
`speedrun readiness` and `speedrun capture` checks may report it offline before
the first prompt. Run `speedrun flush` to start it and deliver queued requests,
or let the first prompt start it. Restart the capture host, send a real prompt
in this repo, then run `speedrun readiness` to check whether it arrived.

Setup writes `.speedrun/project.json` and, with project scope, host adapter
files into the repository. Review `git status` and commit the project config
and any adapter files you want shared; ignore files that should remain local.
Local test evidence requires a clean working tree before and after the command,
and must match the current Git HEAD when verified.

For scripts, specify the choices explicitly, for example:

```bash
speedrun setup --host pi --scope project --capture metadata-only --yes
```

A new project defaults to `any-percent`; use `--category` to choose another.
Once capture succeeds, inspect the run and its milestones:

```bash
speedrun status
speedrun milestone
```

Choose a milestone and follow its available actions. A claim records that a
milestone was reached; a verifier can independently check supported evidence.
Whether a claim counts depends on the category's required trust level. Complete
the required milestones to finish the run. Run `speedrun doctor` if capture is
missing, check that the correct host and project are selected, restart the host,
and send another prompt. `speedrun flush` retries pending writes queued while the
daemon was unavailable; it does not retry permanently rejected requests. See
[troubleshooting](docs/TROUBLESHOOTING.md) for specific symptoms and recovery.

The explicit adapter-only commands below remain available. Claude Code uses a
plugin; VS Code's extension adds a display and tools while Copilot hooks capture
prompts. Zed's extension provides tools but does not capture its own agent prompts.

| Host | Install |
|------|---------|
| Claude Code | `/plugin marketplace add mesh-inc/agentspeedrun` ([details](plugins/claude-code)) |
| OpenCode v2 | `speedrun setup opencode` ([details](plugins/opencode)) |
| pi | `speedrun setup pi` ([details](plugins/pi)) |
| oh-my-pi | `speedrun setup omp` ([details](plugins/pi)) |
| Copilot agent (VS Code Local harness, Copilot CLI) | `speedrun setup copilot --project` ([details](plugins/copilot)) |
| VS Code (timer, splits view, MCP tools) | Build and install the matching source VSIX as described below ([details](plugins/vscode)) |
| Zed (MCP tools) | dev extension in [plugins/zed](plugins/zed) |

In VS Code 1.144, select **Local** in the agent harness picker before sending
a prompt. A test prompt in the newer Copilot SDK harness was not recorded;
its milestone-tool behavior remains unverified. Check `speedrun readiness`
after your first prompt.

When using the source checkout, build its matching VS Code extension (the
published v0.2.0 VSIX may not match the source CLI):

```bash
cd plugins/vscode
npm ci --ignore-scripts
npm run package
code --install-extension agentspeedrun-*.vsix
```

Use `speedrun milestone` to choose a milestone and an action based on its
current state. `speedrun milestone <id>` opens one milestone; `--json` lists the
same actions for integrations. The VS Code **Milestone Actions** command uses
the same choices. The explicit `split`, `accept`, `reject`, `confirm`,
`invalidate`, evidence and verification commands remain available for direct use.
For local test evidence, the terminal action lists recent observations with their
status, time, and whether they match the current Git HEAD. Run
`speedrun local-test -- <command> [args...]` to create an observation. Evidence checks may
fail or be unavailable; attaching evidence does not itself guarantee verification.

```bash
speedrun status                              # timer and splits
speedrun local-test -- cargo test             # record a local test observation
speedrun milestone                           # select the milestone and attach evidence
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
- **Trust levels.** A milestone counts only at its category's required trust: `self_declared` < `agent_claimed` < `evidence_verified`. Only daemon verifiers grant `evidence_verified`: local test observations, git commits in the project's history, live public HTTPS pages (including terms and privacy content), optional passing GitHub Actions runs, and real live-mode Stripe payments.
- **Several clocks.** Real time, timed (minus pauses), agent time (turns, unioned across sessions) and human time (gaps between turns, capped at 15 min each).

Design details: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Building a widget, picture-in-picture or phone view? See [docs/LIVE_DISPLAY.md](docs/LIVE_DISPLAY.md). Runs are local by default. Publication to a configured commons is explicit; [docs/COMMONS.md](docs/COMMONS.md) describes the implemented publication protocol and future proposals.

## Removing the installation

To remove an installed adapter, run `speedrun setup opencode --remove` (or
`pi`, `omp`, or `copilot`); add `--project` if it was installed for a project.
This preserves other host configuration and dependencies. Remove the binaries
installed by the script with:

```bash
curl -fsSL https://raw.githubusercontent.com/MeSH-Inc/agentspeedrun/main/install.sh | sh -s -- --uninstall
```

Use the same `SPEEDRUN_INSTALL_DIR` as installation. The uninstaller stops a
matching daemon and removes only binaries whose contents still match its
installation record. Without that record, including for older or manually
installed binaries, it leaves the binaries untouched. Project settings, run
history, adapters, their dependencies and other configuration remain. Remove adapters before uninstalling the CLI. Extensions
installed through a host's own marketplace must be removed there.

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
