# agentspeedrun for VS Code

A live speedrun timer for agentic coding projects. Tested in VS Code 1.144; requires 1.101+.

- **Status bar timer.** The category, timed clock, splits counted, and how far ahead or behind your personal best you are, e.g. `Any% 0:12:03 · 1/2 · -0:42`. It ticks locally from the daemon's live stream (`docs/LIVE_DISPLAY.md`) without polling.
- **Speedrun view** (Explorer). Milestones with state, split time and PB delta. Accept or reject agent-proposed milestones inline, and split accepted ones.
- **Commands** (`Speedrun: …`): start timing this project, split, add milestone, accept, reject, pause, resume, close run, open the live view in a terminal, and install the Copilot agent hooks.
- **Milestone tools for VS Code's agent.** The extension registers `speedrun mcp` as an MCP server for the open project, so agent mode gets `speedrun_status`, `speedrun_propose_milestones` and `speedrun_claim_milestone`.

## Timing Copilot's agent

VS Code has no extension API for observing chat prompts. Copilot's agent
runs hooks instead. Install them once per repo with **Speedrun: Time
Copilot Agent Prompts**, or from a terminal:

```bash
speedrun setup copilot --project   # writes .github/hooks/agentspeedrun.json; commit it
```

The same hooks time the Copilot CLI. For user-wide hooks, drop `--project`
(`~/.copilot/hooks/`). Claude Code inside VS Code is timed by its own plugin.

## Install

1. Install the `speedrun` and `speedrund` binaries. The extension uses `speedrun` from `PATH`; set `agentspeedrun.speedrunPath` to override.
2. Build and install the extension:

```bash
npm ci --ignore-scripts && npm run package
```

```bash
code --install-extension agentspeedrun-0.1.0.vsix
```

## Develop

```bash
npm run typecheck && npm test   # unit tests (Bun) against the real CLI and daemon
```

```bash
xvfb-run -a npm run test:vscode # integration tests inside a downloaded VS Code
```

Both need `cargo build` first. `src/live.ts` and `src/cli.ts` have no VS Code
imports and are tested directly; `src/extension.ts` is covered by the
integration tests.
