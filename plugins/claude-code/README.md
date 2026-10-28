# agentspeedrun for Claude Code

Thin adapter: hooks forward events to `speedrun`, and the `speedrun mcp`
server gives Claude tools to propose and claim milestones. All state lives
in the local `speedrund` daemon.

| Hook               | Effect                                                        |
|--------------------|---------------------------------------------------------------|
| `UserPromptSubmit` | Records the prompt (redacted) and starts an agent turn. The first prompt starts the run and asks Claude to propose milestones. |
| `Stop`             | Ends the agent turn.                                          |
| `SessionEnd`       | Ends any open turn.                                           |

MCP tools: `speedrun_status`, `speedrun_propose_milestones`, `speedrun_claim_milestone`.

## Install

1. Put `speedrun` and `speedrund` on your `PATH` (`cargo install --path crates/speedrun-cli --path crates/speedrund`, or both binaries from a release).
2. In Claude Code: `/plugin marketplace add mesh-inc/agentspeedrun`, then `/plugin install agentspeedrun@agentspeedrun`.
3. In the repo you want to time: `speedrun init --category any-percent` and commit `.speedrun/project.json`.

Nothing is recorded in directories without `.speedrun/project.json`. Hooks
never block or fail Claude Code; set `SPEEDRUN_DEBUG=1` to see errors.
