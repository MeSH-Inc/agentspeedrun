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
are fixtures. The successful provider-specific checks below supersede the
initial model/tool coverage gap.

A subsequent check resolved the oh-my-pi blocker by using its existing
`openai-codex` provider login rather than assuming Copilot CLI access implied
access from every integrator. Oh-my-pi 18.16.0 with `openai-codex/gpt-6-luna`
completed a real model turn, called `speedrun_status` and
`speedrun_propose_milestones`, and received status and successful proposal
results. The log contained a prompt, turn start, three agent milestone proposals,
and turn end; all 11 events verified as signed, with zero open turns.
The host's supported credential export/import was confined to temporary private
storage and removed afterward.

For Pi 0.87.1, refreshing an isolated copy of its Copilot credentials succeeded,
but actual requests still received `model_not_supported`, including models
listed in the provider catalog. A catalog entry alone is not evidence of model
access for a particular integrator. Pi did not discover oh-my-pi's separate
Codex login and has no documented import path for those credentials; its own
interactive `/login` was used with browser authorization to establish a
separate test login. After authorization, Pi 0.87.1 using
`openai-codex/gpt-6-luna` completed a real turn, invoked `speedrun_status` and
`speedrun_propose_milestones`, and proposed the requested Example milestone.
Its prompt, turn start, agent milestone proposal, and turn end were recorded;
all nine events verified as signed, and `open_turns` was zero. The private
login profile and test data were removed after verification. This successful
check does not establish Copilot integrator access or configure the user's
normal Pi profile.

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

Claude Code 2.1.314 subsequently completed two successful model turns using the
existing authenticated host profile, an explicitly loaded agentspeedrun plugin,
and temporary Git projects with isolated Speedrun data and loopback addresses.
The model invoked `mcp__plugin_agentspeedrun_agentspeedrun__speedrun_status`,
received its result, and completed successfully. The log contained a prompt,
turn start, and Stop/SessionEnd notifications; all nine events were signed and
`speedrun verify` passed. Daemons and temporary project/data directories were
removed afterward. No plugin change or user login was needed.

The earlier 401 came from an isolated stale credential copy; it did not establish
that the active host authentication was unusable. Prefer the host's supported
credential handling over copying access tokens into a test profile. The live
check used the following command from a temporary initialized project with
`SPEEDRUN_HOME` and `SPEEDRUN_ADDR` set to isolated values:

```sh
claude -p --plugin-dir /absolute/path/to/agentspeedrun/plugins/claude-code \
  --allowedTools mcp__plugin_agentspeedrun_agentspeedrun__speedrun_status \
  --output-format stream-json --verbose \
  'Call the agentspeedrun speedrun_status tool exactly once and briefly report its result.'
```

Afterward, inspect `speedrun log`, run `speedrun verify`, and stop the temporary
daemon with `speedrun daemon stop`. This command makes a real model request
using the host account; the temporary Speedrun data does not isolate the host's
own authentication profile.

## Hardware and remaining coverage

The renewed hardware inventory found an Apple M4 Pro Mac, a Linux x86-64 host with an AMD Ryzen 7 8745HS, and a Windows x64 host with an AMD Ryzen AI 7 PRO 450. The owner confirmed that no Intel machine is available. No native Intel smoke test was run; the existing Rosetta and native AMD results do not establish Intel hardware coverage. A native Intel macOS build and packaged binary smoke test remain necessary before claiming that platform was tested. Native Zed agent prompt capture is unsupported by design. Its installed GUI has no headless agent invocation in the available CLI; a real Zed model/tool session was not exercised. Claude Code, Pi and oh-my-pi now have successful real model/tool coverage in the versions and provider configurations recorded above. Other provider configurations are not implied by those results.
