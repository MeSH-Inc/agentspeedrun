# Live host testing

`scripts/live-host.py` is an opt-in check that sends a real prompt to an installed
agent and may consume account credits. It creates a temporary Git project,
Speedrun data directory and loopback address. It asks the model to call both
`speedrun_status` and `speedrun_propose_milestones`, then checks structured host
events for successful results and model completion. It checks the signed event
chain, prompt, turn start and end, the requested agent-generated milestone, and
`open_turns == 0`. Host output is captured in memory and is not printed; this
avoids publishing credentials or full transcripts. The temporary daemon is
stopped and its data removed afterward.

Build the binaries first, then select a host and, when necessary, a model that
its own account can use:

```sh
cargo build
python3 scripts/live-host-test.py
scripts/live-host.py --host claude --model haiku
scripts/live-host.py --host copilot
scripts/live-host.py --host opencode --model opencode/big-pickle
scripts/live-host.py --host pi --model openai-codex/gpt-6-luna
scripts/live-host.py --host omp --model openai-codex/gpt-6-luna
```

`--bindir /path/to/bin` selects the `speedrun` and `speedrund` binaries;
`--timeout 180` controls each command's time limit in seconds. Adapter source
for Claude and pi/omp is loaded explicitly from this checkout; rebuild first
so the other adapters embedded in `speedrun` match it. Exit 0 means all checks
passed, 1 means the check failed, and 77 means the executable was unavailable.
A provider error, inaccessible model, missing tool call, failed tool result,
timeout, or unrecognized transcript is a failure even if the host exits zero.
The harness deliberately does not print the provider's error message; use the
host's own diagnostics separately to investigate. Do not paste credentials or
raw transcripts into reports.

The hosts have separate authentication and isolation mechanisms:

- Claude uses its existing host profile and explicitly loads the checkout's
  plugin. Its normal host settings and authentication remain available.
- Copilot uses a temporary `COPILOT_HOME`, including trust for only the temporary
  project and a session-only MCP configuration. It uses an already exported
  Copilot/GitHub token, or asks the installed `gh` CLI for its authenticated
  token, passing it only in the subprocess environment. No token is printed.
- OpenCode uses its existing authentication, with a
  private standalone server and a project-scoped adapter. The test points its
  config directory at the temporary project to avoid loading personal plugins.
  OpenCode v2 may call registered tools through its `execute` wrapper. For that
  case a temporary forwarding shim records only each successful, recognized
  Speedrun tool name after checking the response; it does not record arguments
  or response content. The harness also requires successful host completion and
  the matching signed agent proposal.
- Pi and oh-my-pi use their own existing authenticated profiles and explicitly
  load the adapter with extension and skill discovery disabled. Sessions are
  not saved by the test. `--model` must name a provider available to that host.

These hosts may retain their own ordinary caches or settings in their existing
profiles. The test isolates Speedrun state and the project, not all host state.
It never copies credentials from one host into another or performs interactive
authorization. If a host needs device login, perform its supported login
separately before running the check. In particular, a provider listed in a model
catalog does not prove the account or integration may use it.

This check exercises CLI agent sessions only. It does not establish VS Code
Copilot UI hook behavior, Zed agent behavior, other host versions or other
providers. See [HOST_VERIFICATION.md](HOST_VERIFICATION.md) for dated manual
coverage. Fixture tests verify rejection behavior but cannot establish live
host compatibility.

On 2026-11-02 the harness passed real Claude Code (Haiku), Copilot CLI (default
model), OpenCode 2.2.5 (`opencode/big-pickle`), and oh-my-pi
(`openai-codex/gpt-6-luna`) sessions with both tools and the signed event checks.
Pi's separate Codex login was unavailable to this run. Copilot's explicit
`gpt-6-luna` selection was unavailable to the isolated token profile, although
its default model succeeded. These results cover those configurations only.
