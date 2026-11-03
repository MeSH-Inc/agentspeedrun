# Host verification

This is a record of local checks on 2026-11-02, not a compatibility guarantee for all host versions.

## VS Code

On Apple silicon macOS, `npm run test:vscode` in `plugins/vscode` passed all 10 tests in the downloaded VS Code 1.144.1 extension host. The runner uses a temporary Git project, temporary `SPEEDRUN_HOME`, loopback daemon and the built `target/debug` binaries. It disables other extensions and uses the test runner's separate VS Code profile. The tests cover extension commands, capture guidance, live display, milestones, history and MCP server registration. They do not submit a prompt through Copilot or test an agent model.

Run it after `cargo build`:

```sh
(cd plugins/vscode && npm run test:vscode)
```

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

An isolated oh-my-pi 18.13.2 startup was also attempted, but it stopped before the RPC check because its clean profile had no default model. That is not a successful adapter test.

## Hardware and remaining coverage

The local Mac is arm64. The reachable Linux x86-64 machine and Windows x64 machine have AMD processors; neither is native Intel macOS. No native Intel Mac was available for this round. A native Intel macOS build and packaged binary smoke test remain necessary before claiming that platform was tested. The installed Claude Code, opencode and Zed hosts were identified but were not exercised with agent prompts in this round. Real agent prompt, turn-end and tool calls remain to be checked for each supported capture host using an authorized model and an isolated project.
