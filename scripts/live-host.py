#!/usr/bin/env python3
"""Opt-in, real-model adapter check. See docs/LIVE_HOST_TESTING.md."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import shutil
import socket
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
PROMPT = ('Call speedrun_status, then call speedrun_propose_milestones to propose '
          'one milestone titled Live Host Example. Briefly report both results. '
          'Do not change files or use other tools.')
TOOLS = {'speedrun_status', 'speedrun_propose_milestones'}


def objects(text):
    for line in text.splitlines():
        try:
            obj = json.loads(line)
            if isinstance(obj, dict):
                yield obj
        except json.JSONDecodeError:
            continue


def run_host(command, cwd, env, timeout):
    """Bound a host and any children it leaves in its own process group."""
    process = subprocess.Popen(command, cwd=cwd, env=env, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=(os.name == 'posix'))
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    except subprocess.TimeoutExpired:
        if os.name == 'posix':
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        else:
            process.kill()
        process.communicate()
        raise


def validate_transcript(host, text, trace=''):
    """Require a successful tool response and completed model, not just exit 0."""
    events = list(objects(text))
    if host == 'claude':
        uses = [b for e in events for b in e.get('message', {}).get('content', [])
                if isinstance(b, dict) and b.get('type') == 'tool_use' and
                b.get('name', '').split('__')[-1] in TOOLS]
        ids = {b['id'] for b in uses}
        results = [b for e in events for b in e.get('message', {}).get('content', [])
                   if isinstance(b, dict) and b.get('type') == 'tool_result' and
                   b.get('tool_use_id') in ids and not b.get('is_error', False)]
        success = any(e.get('type') == 'result' and e.get('subtype') == 'success' and
                      not e.get('is_error', False) for e in events)
        successful_ids = {b['tool_use_id'] for b in results}
        return bool(success and {b['name'].split('__')[-1] for b in uses
                                 if b['id'] in successful_ids} == TOOLS)
    # Hosts expose different JSON event schemas. Require a named tool call and
    # successful result in the same stream, plus a successful final assistant.
    if host in ('pi', 'omp'):
        calls = [e for e in events if e.get('type') == 'tool_execution_start' and
                 e.get('toolName') in TOOLS]
        ids = {e.get('toolCallId') for e in calls}
        results = [e for e in events if e.get('type') == 'tool_execution_end' and
                   e.get('toolCallId') in ids and not e.get('isError', True)]
        ends = [e for e in events if e.get('type') == 'message_end' and
                e.get('message', {}).get('role') == 'assistant' and
                e.get('message', {}).get('stopReason') in ('stop', 'end_turn')]
        successful_ids = {e.get('toolCallId') for e in results}
        return bool(ends and {e['toolName'] for e in calls
                              if e.get('toolCallId') in successful_ids} == TOOLS)
    if host == 'opencode':
        results = [e for e in events if e.get('type') == 'tool_use' and
                   e.get('part', {}).get('tool') in TOOLS and
                   e.get('part', {}).get('state', {}).get('status') == 'completed']
        success = any(e.get('type') == 'step_finish' and
                      e.get('part', {}).get('reason') in ('stop', 'end_turn') for e in events)
        direct = {e['part']['tool'] for e in results} == TOOLS
        wrapped = any(e.get('type') == 'tool_use' and e.get('part', {}).get('tool') == 'execute' and
                      e.get('part', {}).get('state', {}).get('status') == 'completed' for e in events)
        return bool(success and (direct or (wrapped and set(trace.splitlines()) == TOOLS)))
    calls = [e.get('data', {}) for e in events if e.get('type') == 'tool.execution_start']
    completed = [e.get('data', {}) for e in events if e.get('type') == 'tool.execution_complete']
    ids = {e.get('toolCallId') for e in completed if e.get('success') is True}
    names = {e.get('mcpToolName') for e in calls
             if e.get('toolCallId') in ids and e.get('mcpServerName') == 'agentspeedrun'}
    success = any(e.get('type') == 'assistant.idle' for e in events) and not any(
        e.get('type') in ('session.error', 'assistant.error') for e in events)
    return success and TOOLS <= names


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--host', required=True, choices=['claude', 'copilot', 'opencode', 'pi', 'omp'])
    p.add_argument('--model', help='host-specific provider/model or model name')
    p.add_argument('--bindir', type=Path, default=ROOT / 'target/debug')
    p.add_argument('--timeout', type=int, default=180)
    args = p.parse_args()
    exe = shutil.which(args.host)
    if not exe:
        print(f'SKIP {args.host}: executable unavailable')
        return 77
    bindir = args.bindir.resolve()
    binary = bindir / 'speedrun'
    if not binary.is_file() or not (bindir / 'speedrund').is_file():
        p.error('bindir must contain speedrun and speedrund')
    def version(command):
        try:
            output = subprocess.run(command, capture_output=True, text=True, timeout=10).stdout.strip()
            # Versions are reported only when they are simple printable identifiers.
            return output if re.fullmatch(r'[\w .+()-]{1,100}', output) else 'unavailable'
        except (OSError, subprocess.TimeoutExpired):
            return 'unavailable'
    print(f'host={args.host} version={version([exe, "--version"])} '
          f'model={args.model or "host-default"} speedrun={version([str(binary), "--version"])}', flush=True)
    with tempfile.TemporaryDirectory(prefix='speedrun-live-') as tmp:
        root = Path(tmp)
        project = root / 'project'
        project.mkdir()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            address = f'127.0.0.1:{sock.getsockname()[1]}'
        env = {k: v for k, v in os.environ.items() if not k.startswith('SPEEDRUN_')}
        env.update(SPEEDRUN_HOME=str(root / 'data'), SPEEDRUN_ADDR=address,
                   SPEEDRUN_BIN=str(binary), PWD=str(project),
                   PATH=str(bindir) + os.pathsep + env.get('PATH', ''))
        def run(command, **kwargs):
            return subprocess.run(command, cwd=project, env=env, capture_output=True,
                                  text=True, timeout=args.timeout, **kwargs)
        def sr(*command):
            result = run([str(binary), *command])
            if result.returncode:
                raise RuntimeError('speedrun ' + ' '.join(command) + ' failed')
            return result.stdout
        try:
            subprocess.run(['git', 'init', '-q', str(project)], check=True, capture_output=True)
            subprocess.run(['git', '-c', 'user.name=Live Host Test', '-c',
                            'user.email=live-host@example.invalid', 'commit', '-q',
                            '--allow-empty', '-m', 'Initialize'], cwd=project,
                           env=env, check=True, capture_output=True)
            sr('init')
            if args.host in ('copilot', 'opencode'):
                sr('setup', args.host, '--project')
            if args.host == 'opencode':
                # Keep personal configuration/plugins out of the private server;
                # its normal data directory still supplies its own authentication.
                env['XDG_CONFIG_HOME'] = str(root / 'config')
                env['OPENCODE_CONFIG_DIR'] = str(project / '.opencode')
                shim = root / 'speedrun-shim'
                shim.write_text('''#!/usr/bin/env python3
import os, subprocess, sys
result = subprocess.run([os.environ['LIVE_HOST_BINARY'], *sys.argv[1:]], input=sys.stdin.buffer.read(), capture_output=True)
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
if len(sys.argv) >= 3 and sys.argv[1] == 'tool' and result.returncode == 0:
    name = sys.argv[2]
    output = result.stdout.decode(errors='replace')
    valid = (name == 'speedrun_status' and ('any-percent' in output or 'No active run yet' in output)) or (name == 'speedrun_propose_milestones' and 'Proposed 1 milestones.' in output)
    if valid:
        with open(os.environ['LIVE_HOST_TRACE'], 'a') as trace:
            trace.write(name + '\\n')
sys.exit(result.returncode)
''')
                shim.chmod(0o700)
                env.update(SPEEDRUN_BIN=str(shim), LIVE_HOST_BINARY=str(binary),
                           LIVE_HOST_TRACE=str(root / 'tool-trace'))
            if args.host == 'claude':
                cmd = [exe, '-p', '--plugin-dir', str(ROOT / 'plugins/claude-code'),
                       '--allowedTools', 'mcp__plugin_agentspeedrun_agentspeedrun__speedrun_status,mcp__plugin_agentspeedrun_agentspeedrun__speedrun_propose_milestones',
                       '--output-format', 'stream-json', '--verbose']
            elif args.host == 'copilot':
                env['COPILOT_HOME'] = str(root / 'copilot')
                copilot_home = Path(env['COPILOT_HOME'])
                copilot_home.mkdir()
                (copilot_home / 'config.json').write_text(json.dumps({'trustedFolders': [str(project)]}))
                if not any(env.get(k) for k in ('COPILOT_GITHUB_TOKEN', 'GH_TOKEN', 'GITHUB_TOKEN')) and shutil.which('gh'):
                    token = run(['gh', 'auth', 'token'])
                    if token.returncode == 0 and token.stdout.strip():
                        env['COPILOT_GITHUB_TOKEN'] = token.stdout.strip()
                mcp = json.dumps({'mcpServers': {'agentspeedrun': {'type': 'local',
                                  'command': str(binary), 'args': ['mcp'], 'tools': ['*']}}})
                cmd = [exe, '-p', PROMPT, '--allow-all-tools', '--output-format', 'json',
                       '--no-auto-update', '--additional-mcp-config', mcp]
            elif args.host == 'opencode':
                cmd = [exe, 'run', '--standalone', '--format', 'json']
            else:
                cmd = [exe, '-p', '--mode', 'json', '--no-session', '--no-extensions',
                       '--no-skills', '--no-context-files',
                       '--no-builtin-tools' if args.host == 'pi' else '--no-tools',
                       '--extension', str(ROOT / 'plugins/pi/agentspeedrun.ts')]
            if args.model:
                cmd += ['--model', args.model]
            if args.host != 'copilot':
                cmd += [PROMPT]
            result = run_host(cmd, project, env, args.timeout)
            trace_file = root / 'tool-trace'
            trace = trace_file.read_text() if trace_file.exists() else ''
            if result.returncode or not validate_transcript(args.host, result.stdout, trace):
                output = (result.stdout + result.stderr).lower()
                auth = any(s in output for s in ('not authenticated', 'not logged in',
                           'no api key', 'no credentials', 'authentication required',
                           'unauthorized', 'please log in', 'please login'))
                category = 'authentication unavailable or rejected' if auth else 'model, tool, or transcript failure'
                print(f'FAIL {args.host}: {category} (exit {result.returncode}); '
                      'host did not complete both successful tool calls and the model turn')
                return 1
            log = sr('log')
            status = json.loads(sr('status', '--json'))
            verify = sr('verify')
            payloads = [e.get('payload', {}) for e in objects(log)]
            required = {'prompt_submitted', 'agent_turn_started', 'agent_turn_ended'}
            proposal = any(p.get('type') == 'milestone_proposed' and p.get('origin') == 'agent_generated'
                           and p.get('title') == 'Live Host Example' for p in payloads)
            if not required <= {p.get('type') for p in payloads} or not proposal or status.get('open_turns') != 0 or not verify.startswith('ok') or '0 unsigned' not in verify:
                raise RuntimeError('missing prompt/turn events, open turn, or failed chain verification')
            print(f'PASS {args.host}: successful model and status/proposal tools; prompt/start/proposal/end, closed turn, signed chain verified')
            return 0
        except (RuntimeError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
            print(f'FAIL {args.host}: {error}')
            return 1
        finally:
            try:
                run([str(binary), 'daemon', 'stop'])
            except (OSError, subprocess.TimeoutExpired):
                pass


if __name__ == '__main__':
    sys.exit(main())
