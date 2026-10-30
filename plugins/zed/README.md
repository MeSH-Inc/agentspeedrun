# agentspeedrun for Zed

A Zed extension that runs `speedrun mcp` as a context server, so Zed's agent
gets `speedrun_status`, `speedrun_propose_milestones` and
`speedrun_claim_milestone`.

## What gets timed

- **Prompts to Zed's own agent: no.** Zed's extension API only lets extensions provide context servers; it cannot observe prompts. Runs still time from prompts sent by other hosts, and Zed's agent can propose and claim milestones.
- **Agents Zed runs over ACP: yes.** Claude Code and the Copilot CLI are timed by their own hooks (the Claude Code plugin, `speedrun setup copilot`), wherever they run.

## Install

Until it is published, install it as a dev extension: in Zed, run
`zed: install dev extension` and choose this directory. That needs Rust with
the `wasm32-wasip2` target.

Then run `speedrun init` in each project you want timed.

## Settings

```json
{
  "context_servers": {
    "agentspeedrun": {
      "settings": {
        "speedrun_path": "/Users/me/.cargo/bin/speedrun",
        "project_dir": "/Users/me/code/my-app"
      }
    }
  }
}
```

- `speedrun_path`: use it when Zed's `PATH` lacks the CLI, which is common for GUI apps on macOS.
- `project_dir`: only needed if the tools report "not in a speedrun project". By default the server finds the project from MCP roots, then from the directory Zed starts it in. Put it in the project's `.zed/settings.json`.

Without the extension, you can add the same server directly as a custom
context server: command `speedrun`, args `["mcp"]`.

## Develop

```bash
cargo test && cargo build --release --target wasm32-wasip2
```

Zed plans to replace MCP extensions with the official MCP registry. When
that lands, publish `speedrun mcp` there and retire this extension.
