# Architecture

## Goals

1. **Never lose a prompt or a split**, even when the daemon is down or a host crashes.
2. **Results are credible**: history cannot be silently edited and claims carry an explicit trust level.
3. **Rules can evolve** without invalidating old runs.
4. **Hosts are cheap to add**: adapters are a few lines that call `speedrun`.
5. **Private by default**: data stays on the machine; secrets are redacted before storage.

## Components

```
host (Claude Code, opencode, pi/omp, VS Code, Zed)
  └─ adapter: prompts/turns → `speedrun hook claude-code …` / `speedrun ingest …`
              agent tools   → `speedrun mcp` (stdio) / `speedrun tool <name>`
speedrun (CLI)
  ├─ finds the project: nearest .speedrun/project.json (untracked dirs record nothing)
  ├─ HTTP client → speedrund, autostarting it if needed
  └─ spool: $SPEEDRUN_HOME/spool/pending.jsonl when the daemon is unreachable
displays (widgets, PiP, `speedrun watch`)
  └─ /v1/live + /v1/live/stream (SSE) with the read-only display token — see LIVE_DISPLAY.md
speedrund (daemon, 127.0.0.1:7477)
  ├─ HTTP API /v1 (full token in $SPEEDRUN_HOME/token; read-only display-token; both 0600)
  ├─ Service: validation, run lifecycle, verifiers, completion
  └─ Store: SQLite (WAL, synchronous=FULL), append-only events table
```

`$SPEEDRUN_HOME` defaults to `$XDG_DATA_HOME/agentspeedrun` or `~/.local/share/agentspeedrun`.

## Event log

Every run is an ordered, gapless, hash-chained list of events
(`speedrun-core/src/event.rs`). Clients send `event_id` (idempotency key),
`occurred_at` and `payload`; the daemon assigns `seq`, `recorded_at`,
`device_id`, `prev_hash` and `hash`.

- `hash = sha256(canonical_json(event without hash))`, canonical JSON sorting keys recursively.
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

## Timing

All clocks are computed by `RunView::fold` (`projection.rs`) from event times:

| Clock     | Definition |
|-----------|------------|
| `wall_ms` | First prompt → now (or completion). |
| `timed_ms`| `wall_ms` minus explicit pauses. |
| `agent_ms`| Union of agent-turn intervals across all sessions, minus pauses. Concurrent agents count once. |
| `human_ms`| Gaps outside agent turns and pauses, each capped at 15 min (beyond that, the human is away). |

A milestone's split is the clocks at the moment it first met its required trust.

## Trust and verification

`self_declared < agent_claimed < evidence_verified`. Each category milestone
sets `required_trust`. Verifiers (`speedrund/src/verify.rs`):

| Evidence       | Verifier |
|----------------|----------|
| `git_commit`   | Commit exists and is an ancestor of `HEAD` in the project root. |
| `url`          | Planned: reachable over TLS, content check. |
| `ci_run`       | Planned: GitHub Actions run succeeded for a commit in history. |
| `stripe_event` | Planned: Stripe webhook or API lookup confirming a live-mode payment. |
| `note`         | Context only, never proof. |

Until the planned verifiers exist, milestones that require them cannot reach
`evidence_verified`.

## Host adapters

Adapters only translate host events into CLI calls. They hold no logic of their own, so all hosts behave the same and fixes ship with the binary.

| Host | Prompt / turn start | Turn end | Agent tools | Run-start context |
|---|---|---|---|---|
| Claude Code | `UserPromptSubmit` hook | `Stop`, `SessionEnd` | MCP (`speedrun mcp`) | hook `additionalContext` |
| opencode | `chat.message` | `session.status` idle | native `tool` → `speedrun tool` | system-prompt transform |
| pi / omp | `before_agent_start` | `agent_end` | `registerTool` → `speedrun tool` | injected hidden message |

Shared rules:
- **Only human prompts are recorded.** Subagent sessions are skipped: opencode `parentID`, omp `ctx.agent.kind`.
- **One turn-end per turn.** Adapters track which sessions are busy.
- **Adapters never throw into the host.**
- **Install from the binary.** `speedrun setup <host>` installs the copy of the adapter embedded in the binary (`include_str!`), so adapter and CLI versions cannot drift.

Tests run each adapter against the real CLI and daemon with a fake host (`plugins/*/test`).

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

1. URL, CI and Stripe verifiers.
2. opencode and pi adapters through `speedrun ingest`. VS Code status bar and splits view.
3. Materialized projections, once loading a full log per write becomes a cost (today O(events in run) per append).
4. Prompt encryption at rest.
5. Cloud sync: push events to Postgres with `ON CONFLICT (event_id) DO NOTHING`, verify chains server-side, sign events with a device key, leaderboards per `category@version`.
6. Multiple simultaneous categories per run: any% ⊂ 100% ⊂ first revenue share a timeline.
