# Architecture

## Goals

1. **Never lose a prompt or a split**, even when the daemon is down or a host crashes.
2. **Results are credible**: every event is signed by its device and chained to the one before, so anyone holding the device's public key sees an edit, unless the edit was re-signed with that device's private key, which its owner holds. A run is fixed once it is published, in the commons' public append-only log ([COMMONS.md](COMMONS.md)). Claims carry an explicit trust level.
3. **Rules can evolve** without invalidating old runs.
4. **Hosts are cheap to add**: adapters are a few lines that call `speedrun`.
5. **Private by default**: data stays on the machine; secrets are redacted before storage.

## Components

```
host (Claude Code, opencode, pi/omp, Copilot, VS Code, Zed)
  └─ adapter: prompts/turns → `speedrun hook claude-code|copilot …` / `speedrun ingest …`
              agent tools   → `speedrun mcp` (stdio) / `speedrun tool <name>`
              display       → /v1/live/stream (VS Code status bar and splits view)
speedrun (CLI)
  ├─ finds the project: nearest .speedrun/project.json (untracked dirs record nothing)
  ├─ HTTP client → speedrund, autostarting it if needed
  └─ outbox: $SPEEDRUN_HOME/spool/pending.sqlite3 holds writes until acknowledged or rejected
displays (widgets, PiP, `speedrun watch`)
  └─ /v1/live + /v1/live/stream (SSE) with the read-only display token — see LIVE_DISPLAY.md
speedrund (daemon, 127.0.0.1:7477)
  ├─ HTTP API /v1 (full token in $SPEEDRUN_HOME/token; read-only display-token; both 0600)
  ├─ Service: validation, run lifecycle, verifiers, completion
  └─ Store: SQLite (WAL, synchronous=FULL), append-only events table
```

`$SPEEDRUN_HOME` defaults to `$XDG_DATA_HOME/agentspeedrun` or `~/.local/share/agentspeedrun`.

The CLI commits each durable request to its SQLite outbox before sending it.
A flusher leases pending rows transactionally and retries expired leases after a
crash. Unknown outcomes are retried with the original event ID and credential;
acknowledged rows are removed and rejected rows are retained within a size cap.
Storage failures are reported through the spool failure counter. Legacy JSONL
spools are imported transactionally on flush.

## Event log

Every run is an ordered, gapless, hash-chained list of events
(`speedrun-core/src/event.rs`). Clients send `event_id` (idempotency key),
`occurred_at` and `payload`; the daemon assigns `seq`, `recorded_at`,
`device_id`, `prev_hash` and `hash`.

- `hash = sha256(canonical_json(event without hash and sig))`, canonical JSON sorting keys recursively.
- `sig = ed25519(hash)` using the device key in `$SPEEDRUN_HOME/device.key` (mode 0600, generated on first start). `RunStarted` carries `device_pubkey`, so a chain can be checked without the machine that wrote it: an edit that repairs the hash still fails the signature. An event without a signature, or a run that names no device key, fails verification (v1 logs are retired, D4).
- SQLite triggers abort any `UPDATE` or `DELETE` on `events`.
- Retrying an `event_id` returns the stored event; reusing one for a different payload is a 409.
- `occurred_at` more than 5 minutes in the future is clamped. Past times are accepted, since spooled events arrive late.
- `schema_version` is on every event. Additive changes keep the version; breaking changes bump it, and folds handle every version they have ever written.

### Lifecycle

- A run starts with the first `PromptSubmitted` for a project with no open run. The daemon writes `RunStarted` (pinning `category_id@version`), then `MilestoneProposed` + `MilestoneAccepted` for each category milestone, then the prompt. The log is self-contained: it never needs the category file to be understood.
- Agents propose milestones (`origin: agent_generated`); they stay `proposed` until the user accepts them. Users add milestones that are accepted immediately.
- `MilestoneReached` carries a claimed trust (`self_declared` from the CLI, `agent_claimed` from MCP) and evidence. Clients cannot claim `evidence_verified` or send `MilestoneVerified`; only daemon verifiers can.
- When every category (predefined) milestone counts, the daemon appends `RunFinished{completed}`, timestamped at the moment the last one counted. Completed runs are frozen: further prompts are still logged, but milestones cannot change.
- `speedrun reset` appends `RunFinished{abandoned}`. On a completed run this only closes it and keeps the result. The next prompt starts a new run.

Derived events (verification, completion) use deterministic event ids
(`<cause>:verify`, `<run>:completed`) and are re-derived on retries, so a
crash between a claim and its verification heals on the next retry.

Evidence checks can take seconds on the network, so they run outside the
store lock: `Service::append` returns the claim plus a verification job,
the HTTP layer runs it on the blocking pool (to completion, even if the
client hangs up) and `finish_verification` appends `MilestoneVerified`.
The response's `verification` field says what passed or why not. A failed
check is not logged; the claim keeps its trust and claiming again re-checks.
A verification finishing within 10 minutes of its claim is dated at the
claim; a later one (a spooled claim replayed hours on) is dated when the
evidence was actually checked.

A milestone's split is the clocks at the moment it first met its required trust.

## Trust and verification

`self_declared < agent_claimed < evidence_verified`. Each category milestone
sets `required_trust`. Verifiers (`speedrund/src/verify.rs`):

| Evidence       | Verifier |
|----------------|----------|
| `git_commit`   | Commit exists and is an ancestor of `HEAD` in the project root. |
| `url`          | `https://` on a public domain (no IP literals, local names or tunnels such as ngrok), resolves only to public addresses, answers 2xx after at most 5 redirects that stay on HTTPS, and is not a placeholder (server default page, parked domain, lorem ipsum). |
| `ci_run`       | A GitHub Actions run URL of a repo that is a git remote of the project; the run completed with `success`, on a commit in the project's history, and started after the speedrun did. Token: `speedrun secret set github`, `GITHUB_TOKEN`/`GH_TOKEN`, or `gh auth token`. |
| `stripe_event` | A Stripe event, charge, PaymentIntent, Checkout Session or invoice id, read back with the user's restricted live-mode key (`speedrun secret set stripe`): live mode, succeeded and captured, not refunded or disputed, paid after the speedrun started, and not paid from an email address that has committed to the repo (case, a `+tag` and Gmail dots are ignored; another address of the same person still passes). A refund above 10% of the captured amount, or a dispute, fails the check. An event is re-read through its payment object, so a refund made before the check counts; one made after it is not seen, because evidence is checked once, when it is claimed. |
| `note`         | Context only, never proof. |

Every attached item must pass. Some category keys also require a kind of
evidence, whatever trust the claim was made with:

| Key | Needs |
|-----|-------|
| `deployed` | a `url` |
| `legal-pages` | `url`s that together cover terms of service and a privacy policy, each with at least 200 words of server-rendered text |
| `tests-green` | a `ci_run` |
| `first-payment` | a `stripe_event` |

Verifier fetches go out with a fixed user agent, a 15 s timeout and a 2 MB
body cap; nothing fetched is stored, only the references the client sent.
Credentials live in `$SPEEDRUN_HOME/secrets/` (mode 0600), read at check
time, never logged or sent anywhere but their own API (for GitHub, the base
`SPEEDRUN_GITHUB_API` names, by default `https://api.github.com`).

Known gaps: a payment from a friend with an email outside the repo still
counts, and the legal-page check is about presence, not legal adequacy.

## Host adapters

Adapters only translate host events into CLI calls. They hold no logic of their own, so all hosts behave the same and fixes ship with the binary.

| Host | Prompt / turn start | Turn end | Agent tools | Run-start context |
|---|---|---|---|---|
| Claude Code | `UserPromptSubmit` hook | `Stop`, `SessionEnd` | MCP (`speedrun mcp`) | hook `additionalContext` |
| opencode | `chat.message` | `session.status` idle | native `tool` → `speedrun tool` | system-prompt transform |
| pi / omp | `before_agent_start` | `agent_end` | `registerTool` → `speedrun tool` | injected hidden message |
| Copilot agent (VS Code, CLI) | `UserPromptSubmit` hook | `Stop`, `SessionEnd` | MCP (registered by the VS Code extension) | `SessionStart` `additionalContext` |
| Zed agent | — (no API) | — | MCP context server extension | — |

The VS Code extension is a display and control surface: a status bar timer and splits view over the live stream, CLI commands, and an MCP server definition provider. VS Code exposes no prompt API; Copilot's prompts come from its hooks.

`speedrun mcp` finds the project from `--project-dir`, then the client's MCP roots, then its working directory, because editors do not always start servers in the project.

Shared rules:
- **Only human prompts are recorded.** Subagent sessions are skipped: opencode `parentID`, omp `ctx.agent.kind`.
- **One turn-end per turn.** Adapters track which sessions are busy.
- **Adapters never throw into the host.**
- **Install from the binary.** `speedrun setup <host>` installs the copy of the adapter embedded in the binary (`include_str!`), so adapter and CLI versions cannot drift.

Tests run each adapter against the real CLI and daemon: opencode and pi with fake hosts, VS Code inside a real VS Code under Xvfb. The Zed extension's command building is unit-tested, and CI builds it to WASM.

## Live displays

`LiveSnapshot` (`speedrun-core/src/live.rs`) is the only shape displays
consume. It is built from `RunView` plus the personal best: the fastest
earlier completed run of the same `category@version` by `timed_ms`. The
daemon caches completed runs per category and clears the cache whenever a
run finishes. Every successful write bumps a `watch` channel, and stream
subscribers then recompute and send a full snapshot. Details:
[LIVE_DISPLAY.md](LIVE_DISPLAY.md).

## Privacy

- Nothing is recorded outside directories with `.speedrun/project.json`.
- Prompt text passes through `redact()` (API keys, tokens, PEM keys, `*_SECRET=` assignments) before it is stored. `redacted: true` marks changed prompts.
- Displays get the display token, which cannot read events or prompts.
- The daemon only listens on loopback (override: `SPEEDRUN_ALLOW_NON_LOOPBACK=1`) and requires the bearer token.
- Planned: encrypt prompt content at rest with a key in the OS keychain. Make syncing prompt text opt-in and separate from syncing timing metadata.

## Roadmap

1. Materialized projections, once loading a full log per write becomes a cost (today O(events in run) per append).
2. Prompt encryption at rest.
3. Cloud sync: push events to Postgres with `ON CONFLICT (event_id) DO NOTHING`, verify chains server-side, sign events with a device key, leaderboards per `category@version`.
4. Multiple simultaneous categories per run: any% ⊂ 100% ⊂ first revenue share a timeline.
