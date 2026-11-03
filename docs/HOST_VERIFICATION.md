# Host verification

This is a record of local checks on 2026-11-02, not a compatibility guarantee for all host versions.

## VS Code

On Apple silicon macOS, `npm run test:vscode` in `plugins/vscode` passed all 10 tests in the downloaded VS Code 1.144.1 extension host. The runner uses a temporary Git project, temporary `SPEEDRUN_HOME`, loopback daemon and the built `target/debug` binaries. It disables other extensions and uses the test runner's separate VS Code profile. The tests cover extension commands, capture guidance, live display, milestones, history and MCP server registration. They do not submit a prompt through Copilot or test an agent model.

Run it after `cargo build`:

```sh
(cd plugins/vscode && npm run test:vscode)
```

## pi and oh-my-pi actual prompt checks

Pi 0.87.1 and oh-my-pi 18.13.2 each loaded the adapter in an isolated Git
project and submitted a real prompt through the host. Both recorded
`prompt_submitted`, `agent_turn_started`, and `agent_turn_ended`; each event
log verified with all eight events signed. The GitHub Copilot provider rejected
the model requests before producing a response (`model_not_available_for_integrator`
or `model_not_supported`). These checks cover capture and turn closure on
provider failure, not successful model completion or real model tool calls.
The adapter TypeScript check and eight Bun tests also passed; their tool calls
are fixtures. Retest with a model available to each host integrator before
claiming end-to-end model/tool coverage.

## pi startup

pi 0.87.1 loaded `plugins/pi/agentspeedrun.ts` in an actual pi process in offline RPC mode with a temporary home and agent directory. It emitted the adapter's `agentspeedrun` status request and answered `get_commands` successfully. No model or prompt was used, so this checks extension loading, not prompt capture, tool execution or turn completion.

The check was run from the repository root after building the binaries:

```sh
python3 - <<'PY'
import os, subprocess, tempfile
from pathlib import Path
with tempfile.TemporaryDirectory(prefix='speedrun-pi-host-') as tmp:
    repo = Path.cwd()
    env = {k: v for k, v in os.environ.items() if not k.startswith('SPEEDRUN_')}
    env.update(HOME=tmp, PI_CODING_AGENT_DIR=tmp+'/pi',
               SPEEDRUN_HOME=tmp+'/data', XDG_CONFIG_HOME=tmp+'/config',
               XDG_DATA_HOME=tmp+'/xdg-data',
               SPEEDRUN_BIN=str(repo/'target/debug/speedrun'))
    result = subprocess.run(
        ['pi', '--offline', '--no-session', '--no-extensions', '--no-skills',
         '--no-prompt-templates', '--no-themes', '--no-context-files',
         '--extension', str(repo/'plugins/pi/agentspeedrun.ts'), '--mode', 'rpc'],
        input='{"type":"get_commands"}\n', text=True, env=env,
        capture_output=True, timeout=30, check=True)
    print(result.stdout)
    print(result.stderr)
PY
```

An earlier clean-profile oh-my-pi startup had no default model; the subsequent explicit-model prompt checks above supersede that startup-only result.

## OpenCode v2

OpenCode 2.2.5 rejected the v0.3.1 adapter's version-1 plugin API, and a
fresh installation also could not resolve its runtime SDK dependency. The
replacement targets the version-2 plugin API, uses JSON Schema tool definitions
directly, and has no runtime package dependency. Setup leaves existing
`package.json` files untouched. OpenCode version 1 is no longer the target of
this adapter; use the earlier release if version 1 support is required.

Actual isolated OpenCode turns using `opencode/big-pickle` captured signed
prompts and turn-end events, with `open_turns` returning to zero. The model
called `speedrun_status` and, in a second turn, `speedrun_propose_milestones`
with nested array/object arguments and received `Proposed 1 milestones`.
The version-2 prompt admission hook excludes synthetic, shell, compaction and
move messages before invoking the plugin. It does not expose the resolved model,
so the optional prompt model field is unset. Child sessions remain excluded.

## Copilot CLI and Claude Code

Copilot CLI 1.0.93 with GPT-6 Luna completed actual model turns in isolated
projects using both user-scope hooks and trusted project-scope hooks. The model
called the agentspeedrun `speedrun_status` MCP tool. Each run recorded a prompt,
a turn start, and Stop/SessionEnd notifications; all nine events verified as
signed. A fresh untrusted project did not execute project hooks, even when tool
path access was permitted. Admitting only the temporary project through
Copilot's workspace trust configuration enabled capture. The projection ignores
redundant end notifications after the turn closes; a regression test covers it.

Claude Code 2.1.314 loaded its hooks and recorded a signed prompt, but its
isolated credential copy received an expired-token 401. A successful Claude
model response, normal completed turn, and model-invoked MCP tool remain
unverified. Refresh authentication in the host before repeating this check.

## Hardware and remaining coverage

The local Mac is arm64. The reachable Linux x86-64 machine and Windows x64 machine have AMD processors; neither is native Intel macOS. No native Intel Mac was available for this round. A native Intel macOS build and packaged binary smoke test remain necessary before claiming that platform was tested. Native Zed agent prompt capture is unsupported by design. Its installed GUI has no headless agent invocation in the available CLI; a real Zed model/tool session was not exercised. Successful model responses and model-invoked tools still need verification in Claude Code, pi and oh-my-pi with working host authentication and supported model access.
