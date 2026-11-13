# agentspeedrun for opencode

A thin plugin: every behavior lives in the `speedrun` CLI, which this file
calls. Tested against opencode 1.19.

| opencode hook | Effect |
|---|---|
| `chat.message` | Records the prompt (text the user typed, not synthetic parts) with the model, and starts an agent turn. The first prompt starts the run. |
| `experimental.chat.system.transform` | After a run starts, tells the agent about it until the agent proposes milestones. |
| `session.status` idle / `session.idle` | Ends the turn (once per turn). |
| `tool` | Native `speedrun_status`, `speedrun_propose_milestones`, `speedrun_claim_milestone`, backed by `speedrun tool`. |

Subagent sessions (those with a `parentID`) are ignored, so prompts written
by the agent for its subagents are never recorded as yours.

## Install

```bash
speedrun setup opencode            # user-wide: ~/.config/opencode/plugins/
```

```bash
speedrun setup opencode --project  # this repo only: .opencode/plugins/
```

`setup` also adds `@opencode-ai/plugin` to that directory's `package.json`.
opencode installs it at startup. Then, in each repo you want timed, run:

```bash
speedrun init
```

The plugin calls `speedrun` from `PATH`; set `SPEEDRUN_BIN` to override.
Failures never interrupt opencode. Set `SPEEDRUN_DEBUG=1` to see them.

## Develop

```bash
npm ci --ignore-scripts && npx tsc --noEmit && npm test
```

Tests drive the plugin against the real `speedrun` and `speedrund` from
`target/debug`, so run `cargo build` first.
