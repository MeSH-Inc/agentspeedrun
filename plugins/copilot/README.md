# GitHub Copilot agent hooks

`agentspeedrun.json` times prompts to Copilot's agent in VS Code and in the
Copilot CLI, using
[Copilot hooks](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-hooks-reference).
It uses the PascalCase event aliases, which receive Claude-style snake_case
input.

| Event | Effect |
|---|---|
| `SessionStart` | Tells the agent the project is timed and to propose milestones (Copilot ignores output from prompt hooks, so this is the only place to say it). Skipped once the agent has proposed some. |
| `UserPromptSubmit` | Records the prompt and starts a turn; the first prompt starts the run. |
| `Stop`, `SessionEnd` | End the turn. |

Install:

```bash
speedrun setup copilot --project  # .github/hooks/agentspeedrun.json; commit it
```

```bash
speedrun setup copilot            # ~/.copilot/hooks/ ($COPILOT_HOME/hooks/)
```

For the milestone tools, the VS Code extension registers the MCP server
automatically. For the Copilot CLI, add `speedrun mcp` as an MCP server.

In VS Code 1.144, both **Local** and **Copilot** harnesses were verified
with prompt capture and extension-provided milestone tools. Let MCP server
startup finish before using the tools; if they are unavailable, check the MCP
server status. See [live verification](../../docs/VS_CODE_LIVE_VERIFICATION.md).

Copilot CLI runs project hooks only in a trusted workspace. Admit the project
through Copilot’s workspace trust flow before testing capture; permitting tool
paths alone does not enable project hooks. After sending a prompt, run
`speedrun readiness` in the project and check that recording is `captured`.
User-scope hooks and trusted project-scope hooks were exercised with Copilot
CLI 1.0.93; see [host verification](../../docs/HOST_VERIFICATION.md).
