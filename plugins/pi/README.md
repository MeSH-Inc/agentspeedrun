# agentspeedrun for pi and oh-my-pi

One extension for both [pi](https://github.com/badlogic/pi-mono) and its
fork [oh-my-pi](https://github.com/can1357/oh-my-pi). Every behavior lives in
the `speedrun` CLI, which this file calls. Tested against pi 0.73 and omp 18.14.

| Event | Effect |
|---|---|
| `before_agent_start` | Records the prompt with the model and starts an agent turn. On the first prompt of a run, injects a hidden message telling the agent about it. |
| `agent_end`, `session_shutdown` | Ends the turn (once per turn). |
| `session_start`, `agent_end`, claims | Footer status: `⏱ any-percent 1/2 · 0:12:03`. |
| `registerTool` | `speedrun_status`, `speedrun_propose_milestones`, `speedrun_claim_milestone`, backed by `speedrun tool`. |

On oh-my-pi:
- Subagent sessions (`ctx.agent.kind === "sub"`) and subagent revives are ignored, so only your prompts are recorded.
- Prompts are recorded with host `omp`.

## Install

```bash
speedrun setup pi              # ~/.pi/agent/extensions/
```

```bash
speedrun setup omp             # ~/.omp/agent/extensions/
```

Add `--project` to install into this repo's `.pi/extensions/` or
`.omp/extensions/` instead. With pi you can also install from the repo:

```bash
pi install git:github.com/mesh-inc/agentspeedrun
```

Then, in each repo you want timed, run:

```bash
speedrun init
```

The extension calls `speedrun` from `PATH`; set `SPEEDRUN_BIN` to override.
Failures never interrupt the agent. Set `SPEEDRUN_DEBUG=1` to see them.

## Develop

```bash
npm ci --ignore-scripts && npx tsc --noEmit && npm test
```

Tests drive the extension against the real `speedrun` and `speedrund` from
`target/debug`, so run `cargo build` first.
