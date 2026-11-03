# First-use testing of the 0.3.2 preview

These are **agent-simulated first-use sessions**, not a human usability study. They were run on macOS Apple silicon on 2026-11-02 using the native `dist/preview/0.3.2/agentspeedrun-v0.3.2-aarch64-apple-darwin.tar.gz` binaries. Each scenario used a separate temporary home, `SPEEDRUN_HOME`, and Git project. The README and CLI help were the initial guidance. The binary directory was added to `PATH`; this exercised the archive, not the remote installer or its uninstall record. No existing user configuration or credentials were copied or changed.

## Scenario 1: setup through completion

1. Initialized a Git project with an initial commit, then ran `speedrun setup --host copilot --scope project --capture metadata-only --yes`. Setup created and trusted the project and installed `.github/hooks/agentspeedrun.json`.
2. `speedrun readiness` and `speedrun capture` reported daemon offline and directed the user to `speedrun flush`. After `speedrun flush`, readiness reported `awaiting_prompt` and requested a host prompt. Committed the generated project and adapter files as setup suggested.
3. Attempted `copilot -p 'Describe this project briefly' --allow-all-tools --no-ask-user` with the isolated home. Copilot reported no authentication information. To continue without importing credentials, sent a **synthetic** JSON prompt to `speedrun hook copilot user-prompt-submit`. Readiness reported `captured`; status showed one prompt and one session. This tests the hook entry point, not actual Copilot delivery.
4. `speedrun milestone` reported that interactive actions require a terminal and suggested `--json`; `speedrun milestone --json` listed the two available actions. `speedrun split scaffold-runs` and `speedrun split happy-path-demo --commit HEAD` counted both claims and completed the run. The second command correctly distinguished the semantic claim from verified evidence: the commit alone could not verify the demo. `speedrun verify` reported 10 signed events and a valid chain. Stopped the daemon.

## Scenario 2: restart, recovery, and adapter removal

A second clean home, data directory, and Git project used the same explicit setup choices. A synthetic Copilot hook prompt started a run and readiness reported capture. After `speedrun daemon stop`, `speedrun recovery create <bundle>` and `speedrun recovery restore <bundle> <new-data-dir>` succeeded. With `SPEEDRUN_HOME` pointing to the restored directory, `speedrun flush` started the daemon, `speedrun status` showed the original running session, and `speedrun verify` reported seven signed events and a valid chain. Stopped the restored daemon. `speedrun setup copilot --remove --project` removed the generated hook and reported that project data and configuration were preserved.

## Additional real-host capture

A third clean Git project and data directory used `speedrun setup --host claude --capture metadata-only --yes`. Claude Code ran a benign one-word reply prompt with `--plugin-dir` pointing at this checkout’s Claude plugin. It used the existing host profile for authentication, while `SPEEDRUN_HOME` and the project were isolated; this is not a fresh authentication or marketplace-install test. The host exited successfully and `speedrun readiness` reported `captured` by `claude-code`; status showed one prompt and one session. The daemon was then stopped. This establishes actual host-to-hook capture for that local plugin and preview CLI pair.

## Friction and limits

- `speedrun ingest --help` produced `{"recorded":false,"error":"unusable arguments: one or more required arguments were not provided"}` instead of command help; `speedrun hook --help` produced no help. The top-level help describes these as integration entry points, but an agent or adapter author cannot discover their required arguments in the usual way. Both commands intentionally have special always-success behavior. A subsequent source change prints help for explicit help requests while preserving successful JSON responses for malformed ingest arguments; the packaged preview tested above predates that change.
- A clean home had no Copilot authentication, and the sessions used synthetic hooks. Those two Copilot scenarios do not prove that Copilot invokes the adapter, that interactive setup works, or that a real person understands the workflow. The separate Claude session used an existing authenticated profile and the checkout plugin, not a marketplace installation. The archive was extracted locally rather than installed or uninstalled with the release installer.
- The setup-to-completion scenario used self-declared category milestones and a minimal Git project; it does not establish the quality of a real project or all evidence verification paths.

Recommended follow-up: repeat help and malformed-input checks against the next packaged candidate, including install and recorded removal in a disposable home. Observe a new user completing setup, an actual prompt, a milestone, and recovery without coaching. Separately verify VS Code Copilot agent prompt capture and a milestone tool invocation; Copilot CLI and extension-host fixture tests do not prove that GUI path.
