// agentspeedrun for VS Code: a live timer in the status bar, a splits view,
// commands for the speedrun CLI, and the milestone tools for VS Code's agent
// via MCP. Prompt capture for Copilot's agent comes from its hooks
// (`speedrun setup copilot`), not from this extension: VS Code has no API to
// observe chat prompts. Source: plugins/vscode in mesh-inc/agentspeedrun.

import * as vscode from "vscode";
import { Cli, PROJECT_FILE, projectId, type Readiness } from "./cli";
import { fmtDelta, fmtMs, type LiveMilestone, type LiveRun, LiveClient, type LiveSnapshot, type LiveState, statusText } from "./live";

type Project = { folder: vscode.WorkspaceFolder; id: string };

/** Returned from activate() for tests and other extensions. */
export interface Api {
  statusText(): string | undefined;
  run(): LiveRun | undefined;
  mcpServerDefinitions(): vscode.McpStdioServerDefinition[];
}

const BADGE: Record<LiveMilestone["state"], string> = {
  proposed: "question",
  accepted: "circle-large-outline",
  rejected: "circle-slash",
  reached: "circle-large-filled",
  verified: "pass-filled",
};

export async function activate(context: vscode.ExtensionContext): Promise<Api> {
  const config = () => vscode.workspace.getConfiguration("agentspeedrun");
  const bin = () => config().get<string>("speedrunPath") || process.env.SPEEDRUN_BIN || "speedrun";
  const cli = new Cli(bin);

  let project: Project | undefined;
  let snapshot: LiveSnapshot | undefined;
  let receivedAt = new Date();
  let liveState: LiveState = "disconnected";
  let frozenAt = Date.now();
  let warnedMissing = false;
  let readiness: Readiness | undefined;
  let readinessFolder: string | undefined;

  const currentRun = () => snapshot?.runs.find((r) => r.project_id === project?.id);
  const asOf = () => (snapshot ? new Date(Date.parse(snapshot.as_of)) : new Date());
  // Tick from the local clock, shifted so that skew between us and the daemon cancels out.
  const now = () => new Date(asOf().getTime() + ((liveState === "connected" ? Date.now() : frozenAt) - receivedAt.getTime()));

  // --- status bar ---------------------------------------------------------
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  status.command = "agentspeedrun.splits.focus";
  context.subscriptions.push(status);

  const render = () => {
    const run = currentRun();
    if (!readinessFolder) return status.hide();
    if (!readiness) {
      status.text = "$(warning) Speedrun: status unavailable";
      status.tooltip = "Could not read capture readiness from the speedrun CLI.";
      return status.show();
    }
    const details = `Project: ${readiness.project} · Daemon: ${readiness.daemon} · Recording: ${readiness.recording}\nLast activity: ${readiness.last_activity ?? "none observed"} by ${readiness.last_host ?? "unknown host"}\nDevice queue: ${readiness.queued} · Rejected: ${readiness.rejected} · Write failures: ${readiness.write_failures}\n${readiness.next_action}`;
    if (readiness.project !== "trusted" || readiness.daemon !== "online" || readiness.recording === "unknown" || readiness.rejected > 0 || readiness.write_failures > 0 || readiness.queued > 0) {
      status.text = `$(warning) Speedrun: ${readiness.project !== "trusted" ? readiness.project : readiness.daemon !== "online" ? readiness.daemon : "attention needed"}`;
      status.tooltip = details;
      return status.show();
    }
    if (liveState !== "connected") {
      status.text = `$(warning) Speedrun: ${liveState}`;
      status.tooltip = `Live data is unavailable or cannot be trusted. The last timer is frozen.\n${details}`;
      return status.show();
    }
    if (!run) {
      status.text = `$(watch) Speedrun: ${readiness.recording === "captured" ? "capture observed" : (readiness.integrations.length || readiness.selected_integration) ? "waiting for first prompt" : "integration needed"}`;
      status.tooltip = details;
      return status.show();
    }
    status.text = statusText(run, asOf(), now());
    const t = run.timing;
    status.tooltip = new vscode.MarkdownString(
      [
        `**${run.category.name}** — ${run.status}${run.active_agents ? ` · ${run.active_agents} agent(s) working` : ""}`,
        "",
        `timed ${fmtMs(t.timed_ms)} · agent ${fmtMs(t.agent_ms)} · human ${fmtMs(t.human_ms)}`,
        run.pb ? `PB ${fmtMs(run.pb.timing.timed_ms)}` : "",
        "", details,
      ].join("\n"),
    );
    status.show();
  };
  let readinessRequest: Promise<void> | undefined;
  let readinessRequestFolder: string | undefined;
  let readinessRequestedAt = 0;
  async function refreshReadiness(force = false) {
    const folder = readinessFolder;
    if (!folder) return;
    if (readinessRequest) {
      if (readinessRequestFolder === folder) return readinessRequest;
      await readinessRequest;
      if (folder !== readinessFolder) return;
    }
    if (!force && Date.now() - readinessRequestedAt < 10_000) return;
    readinessRequestedAt = Date.now();
    const request = (async () => {
      const result = await cli.readiness(folder);
      if (folder === readinessFolder) { readiness = result; render(); }
    })();
    readinessRequest = request;
    readinessRequestFolder = folder;
    try { await request; } finally { if (readinessRequest === request) { readinessRequest = undefined; readinessRequestFolder = undefined; } }
  }
  const readinessTicker = setInterval(() => { void refreshReadiness(); }, 10_000);
  context.subscriptions.push({ dispose: () => clearInterval(readinessTicker) });
  const ticker = setInterval(render, 1000);
  context.subscriptions.push({ dispose: () => clearInterval(ticker) });

  // --- splits view ---------------------------------------------------------
  const changed = new vscode.EventEmitter<void>();
  const tree: vscode.TreeDataProvider<LiveMilestone> = {
    onDidChangeTreeData: changed.event,
    getChildren: (element) => (element ? [] : (currentRun()?.milestones.filter((m) => m.state !== "rejected") ?? [])),
    getTreeItem: (m) => {
      const item = new vscode.TreeItem(m.title);
      const color = m.counts ? new vscode.ThemeColor("testing.iconPassed") : undefined;
      item.iconPath = new vscode.ThemeIcon(BADGE[m.state], color);
      const split = m.split ? fmtMs(m.split.timed_ms) : "";
      const delta = m.delta_ms != null ? ` ${fmtDelta(m.delta_ms)}` : "";
      item.description =
        m.state === "proposed" ? "proposed by agent" : m.pending_confirmation ? "awaiting confirmation" : m.verification && m.verification.state !== "verified" ? `local check: ${m.verification.state}` : m.counts ? `${split}${delta}` : m.trust ? `needs ${m.required_trust}` : "";
      item.tooltip = `${m.key ?? m.milestone_id} · ${m.origin} · trust ${m.trust ?? "none"} (needs ${m.required_trust})${m.verification ? ` · local check ${m.verification.state}` : ""}`;
      item.contextValue = m.state;
      return item;
    },
  };
  context.subscriptions.push(vscode.window.registerTreeDataProvider("agentspeedrun.splits", tree), changed);

  // --- live stream ----------------------------------------------------------
  const live = new LiveClient(
    async () => {
      if (!project) return undefined;
      const conn = await cli.connection(project.folder.uri.fsPath);
      if (!conn) await warnIfMissing();
      return conn;
    },
    (snap) => {
      const previous = currentRun();
      snapshot = snap;
      const completed = currentRun();
      if (previous && completed && previous.run_id === completed.run_id && previous.status !== "completed" && completed.status === "completed") {
        void vscode.window.showInformationMessage(`Run completed in ${fmtMs(completed.timing.timed_ms)}. ${completed.progress.counted}/${completed.progress.total} category milestones counted.`, "View history").then((choice) => {
          if (choice) void vscode.commands.executeCommand("agentspeedrun.history");
        });
      }
      receivedAt = new Date();
      void refreshReadiness();
      render();
      changed.fire();
    },
    (state) => {
      if (liveState === "connected" && state !== "connected") frozenAt = Date.now();
      liveState = state;
      if (state === "connected") void refreshReadiness(true);
      render();
      changed.fire();
    },
  );
  context.subscriptions.push({ dispose: () => live.stop() });

  async function warnIfMissing() {
    if (warnedMissing) return;
    const probe = await cli.run(["--version"], project?.folder.uri.fsPath ?? process.cwd());
    if (!probe.missing) return;
    warnedMissing = true;
    const pick = await vscode.window.showWarningMessage(
      `agentspeedrun: \`${bin()}\` was not found. Install the speedrun CLI or set its path.`,
      "Set path",
    );
    if (pick) void vscode.commands.executeCommand("workbench.action.openSettings", "agentspeedrun.speedrunPath");
  }

  // --- MCP: milestone tools for VS Code's agent ---------------------------
  const mcpChanged = new vscode.EventEmitter<void>();
  const mcpServerDefinitions = () =>
    project
      ? [new vscode.McpStdioServerDefinition("agentspeedrun", bin(), ["mcp", "--project-dir", project.folder.uri.fsPath])]
      : [];
  if (vscode.lm?.registerMcpServerDefinitionProvider) {
    context.subscriptions.push(
      vscode.lm.registerMcpServerDefinitionProvider("agentspeedrun", {
        onDidChangeMcpServerDefinitions: mcpChanged.event,
        provideMcpServerDefinitions: async () => mcpServerDefinitions(),
      }),
      mcpChanged,
    );
  }

  // --- project detection ----------------------------------------------------
  let started = false;
  async function detect() {
    let found: Project | undefined;
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      const id = await projectId(folder.uri.fsPath);
      if (id) {
        found = { folder, id };
        break;
      }
    }
    const changedProject = found?.id !== project?.id;
    project = found;
    const folder = found?.folder.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (folder !== readinessFolder) { readinessFolder = folder; readiness = undefined; }
    await refreshReadiness(true);
    await vscode.commands.executeCommand("setContext", "agentspeedrun.tracked", Boolean(project));
    if (changedProject) {
      mcpChanged.fire();
      changed.fire();
    }
    if (project && !started) {
      started = true;
      live.start();
    }
    render();
  }
  const watcher = vscode.workspace.createFileSystemWatcher(`**/${PROJECT_FILE}`);
  context.subscriptions.push(
    watcher,
    watcher.onDidCreate(detect),
    watcher.onDidDelete(detect),
    vscode.workspace.onDidChangeWorkspaceFolders(detect),
  );

  // --- commands ---------------------------------------------------------------
  /** Run a CLI command in the project; show its error text on failure. */
  async function speedrun(
    args: string[],
    cwd = project?.folder.uri.fsPath,
    timeoutMs?: number,
  ): Promise<string | undefined> {
    if (!cwd) {
      void vscode.window.showWarningMessage("agentspeedrun: open a folder first.");
      return undefined;
    }
    const r = await cli.run(args, cwd, "", timeoutMs);
    if (r.missing) {
      await warnIfMissing();
      return undefined;
    }
    if (!r.ok) {
      void vscode.window.showErrorMessage(`speedrun ${args[0]}: ${(r.stderr || r.stdout).trim().replace(/^error: /, "")}`);
      return undefined;
    }
    return r.stdout;
  }

  /** A milestone from a tree item, an id/key argument, or a quick pick over `filter`. */
  async function chooseMilestone(arg: unknown, filter: (m: LiveMilestone) => boolean, title: string) {
    if (typeof arg === "string") return arg;
    if (arg && typeof arg === "object" && "milestone_id" in arg) return (arg as LiveMilestone).milestone_id;
    const options = (currentRun()?.milestones ?? []).filter(filter);
    if (!options.length) {
      void vscode.window.showInformationMessage(`agentspeedrun: no milestones to ${title.toLowerCase()}.`);
      return undefined;
    }
    const pick = await vscode.window.showQuickPick(
      options.map((m) => ({ label: m.title, description: m.key ?? m.origin, id: m.milestone_id })),
      { title },
    );
    return pick?.id;
  }

  const register = (id: string, fn: (...args: any[]) => unknown) =>
    context.subscriptions.push(vscode.commands.registerCommand(id, fn));

  register("agentspeedrun.init", async (category?: string) => {
    const folder =
      vscode.workspace.workspaceFolders?.length === 1
        ? vscode.workspace.workspaceFolders[0]
        : await vscode.window.showWorkspaceFolderPick({ placeHolder: "Folder to time" });
    if (!folder) return;
    if (!category) {
      const out = await speedrun(["categories"], folder.uri.fsPath);
      if (!out) return;
      const items = out
        .trim()
        .split("\n")
        .map((line) => {
          const [, id, name, description] = line.match(/^(\S+)\s+v\d+\s+(.+?) — (.*)$/) ?? [];
          return { label: name ?? line, description: id, detail: description, id };
        })
        .filter((i) => i.id);
      category = (await vscode.window.showQuickPick(items, { title: "Speedrun category" }))?.id;
      if (!category) return;
    }
    if (await speedrun(["init", "--category", category], folder.uri.fsPath)) {
      await detect();
      void vscode.window.showInformationMessage(
        "agentspeedrun: tracking this project. Commit .speedrun/project.json. The timer starts at the first agent prompt.",
      );
    }
  });

  // Evidence the daemon can check, by the flag `speedrun split` takes it with.
  const EVIDENCE = [
    { label: "Attach HEAD commit", detail: "Checked against this repo's history.", flag: "--commit", prompt: "" },
    { label: "Public URL…", detail: "The deployment, or a terms/privacy page.", flag: "--url", prompt: "https://" },
    { label: "GitHub Actions run…", detail: "A passing run of this repo.", flag: "--ci", prompt: "https://github.com/" },
    { label: "Stripe payment…", detail: "pi_, ch_, cs_, in_ or evt_ id; needs `speedrun secret set stripe`.", flag: "--stripe", prompt: "" },
    { label: "Local test observation…", detail: "Observation ID from speedrun local-test.", flag: "--local-test", prompt: "" },
    { label: "No evidence", detail: "Counts only for milestones that accept self_declared.", flag: "", prompt: "" },
  ];

  register("agentspeedrun.split", async (arg?: unknown, commit?: string) => {
    const id = await chooseMilestone(arg, (m) => (m.state === "accepted" || m.state === "reached") && !m.counts, "Split");
    if (!id) return;
    let evidence: string[] = [];
    if (commit !== undefined) {
      evidence = commit ? ["--commit", commit] : [];
    } else {
      while (true) {
        const pick = await vscode.window.showQuickPick([...EVIDENCE, { label: "Done", detail: "Submit the selected evidence.", flag: "done", prompt: "" }], { title: `Evidence (${evidence.length / 2} attached)` });
        if (!pick) return;
        if (pick.flag === "done" || !pick.flag) break;
        if (pick.flag !== "--url") {
          const previous = evidence.indexOf(pick.flag);
          if (previous >= 0) evidence.splice(previous, 2);
        }
        if (pick.flag === "--commit") evidence.push("--commit", "HEAD");
        else if (pick.flag === "--local-test") {
          const out = await speedrun(["local-test-history"]);
          if (!out) return;
          try {
            const attempts = JSON.parse(out) as Array<{ id: string; argv: string[]; status: { kind: string }; observed_at: string }>;
            const selected = await vscode.window.showQuickPick(attempts.map((a) => ({ label: `Local test (${a.argv.length} arguments hidden)`, description: a.status.kind, detail: a.observed_at, id: a.id })), { title: "Select a local test observation" });
            if (!selected) return;
            evidence.push(pick.flag, selected.id);
          } catch { void vscode.window.showErrorMessage("Could not read local test observations."); return; }
        }
        else {
          const value = await vscode.window.showInputBox({ title: pick.label.replace("…", ""), value: pick.prompt });
          if (!value?.trim()) return;
          evidence.push(pick.flag, value.trim());
        }
      }
    }
    // The daemon checks evidence before answering; network checks take a few seconds.
    const out = await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: "agentspeedrun: checking evidence…" },
      () => speedrun(["split", id, ...evidence], undefined, 120_000),
    );
    const verdict = out?.split("\n\n")[0];
    if (verdict) void vscode.window.showInformationMessage(verdict);
  });

  register("agentspeedrun.add", async (title?: string) => {
    title ??= await vscode.window.showInputBox({ title: "New milestone", placeHolder: "e.g. Recipes persist across restarts" });
    if (title?.trim()) await speedrun(["add", title.trim()]);
  });

  register("agentspeedrun.accept", async (arg?: unknown) => {
    const id = await chooseMilestone(arg, (m) => m.state === "proposed", "Accept");
    if (id) await speedrun(["accept", id]);
  });

  register("agentspeedrun.reject", async (arg?: unknown) => {
    const id = await chooseMilestone(arg, (m) => m.state === "proposed", "Reject");
    if (id) await speedrun(["reject", id]);
  });

  register("agentspeedrun.confirm", async (arg?: unknown) => {
    const id = await chooseMilestone(arg, (m) => m.pending_confirmation, "Confirm agent claim");
    if (id) await speedrun(["confirm", id]);
  });
  register("agentspeedrun.invalidate", async (arg?: unknown) => {
    const id = await chooseMilestone(arg, (m) => m.state === "reached" || m.state === "verified", "Invalidate milestone");
    if (!id) return;
    const reason = await vscode.window.showInputBox({ title: "Reason for invalidating this milestone" });
    if (reason?.trim()) await speedrun(["invalidate", id, "--reason", reason.trim()]);
  });
  register("agentspeedrun.verification", async (arg?: unknown) => {
    let milestone: LiveMilestone | undefined;
    if (arg && typeof arg === "object" && "milestone_id" in arg) milestone = arg as LiveMilestone;
    else {
      const id = await chooseMilestone(undefined, (m) => Boolean(m.verification), "Select a local evidence check");
      milestone = currentRun()?.milestones.find((m) => m.milestone_id === id);
    }
    if (!milestone?.verification) { void vscode.window.showInformationMessage("No local evidence check for this milestone."); return; }
    const out = await speedrun(["verification", milestone.verification.cause_event_id]);
    if (out) {
      const status = JSON.parse(out);
      const report = status.report;
      await showText("Local evidence check", [`${milestone.title}`, `State: ${status.state} (${status.attempts} attempts)`, report ? `Scope: ${report.scope} · Observer: ${report.observer} ${report.observer_version}` : "Waiting for a result.", ...(report?.problems ?? []), ...(report?.facts ?? []).map((fact: string) => `Observed: ${fact}`)].join("\n"));
    }
  });
  register("agentspeedrun.localTest", async () => {
    const input = await vscode.window.showInputBox({ title: "Run local test", prompt: "Enter the command and arguments as a JSON array, for example [\"cargo\", \"test\"]" });
    if (!input) return;
    let argv: string[];
    try { argv = JSON.parse(input); if (!Array.isArray(argv) || !argv.length || argv.some((v) => typeof v !== "string" || !v)) throw new Error(); }
    catch { void vscode.window.showErrorMessage("Enter a nonempty JSON array of command arguments."); return; }
    const out = await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: "Running local test" }, () => speedrun(["local-test", "--", ...argv], undefined, 180_000));
    if (out) { const result = JSON.parse(out); void vscode.window.showInformationMessage(`Local test: ${result.status.kind}. Select this observation when attaching evidence.`); }
  });
  register("agentspeedrun.capture", async () => {
    const out = await speedrun(["capture"]);
    if (out) void vscode.window.showInformationMessage(out.trim());
  });
  async function chooseRun(): Promise<string | undefined> {
    const out = await speedrun(["history", "--json"]);
    if (!out) return undefined;
    try {
      const runs = JSON.parse(out) as Array<{ run_id: string; category_id: string; category_version: number; status: string; started_at: string }>;
      if (!runs.length) { void vscode.window.showInformationMessage("No runs captured for this project yet."); return undefined; }
      return (await vscode.window.showQuickPick(runs.map((r) => ({ label: `${r.category_id}@${r.category_version} · ${r.status}`, description: r.started_at, detail: r.run_id, id: r.run_id })), { title: "Select a run" }))?.id;
    } catch { void vscode.window.showErrorMessage("Could not read run history."); return undefined; }
  }
  async function showText(title: string, content: string) {
    const doc = await vscode.workspace.openTextDocument({ language: "plaintext", content });
    await vscode.window.showTextDocument(doc, { preview: false });
    return title;
  }
  register("agentspeedrun.history", async (runId?: string) => {
    const id = runId ?? await chooseRun();
    if (!id) return;
    const out = await speedrun(["inspect", id]);
    if (out) await showText("Run details", out);
  });
  register("agentspeedrun.publish", async () => {
    const id = await chooseRun();
    if (!id) return;
    const tier = await vscode.window.showQuickPick([
      { label: "Tier 0: chain heads", value: "0" },
      { label: "Tier 1: signed event log", value: "1" },
      { label: "Tier 2: signed event log and prompt commitments", value: "2" },
    ], { title: "Choose disclosure tier" });
    if (!tier) return;
    const summary = await speedrun(["inspect", id]);
    if (!summary) return;
    const preview = await speedrun(["publish", "--run", id, "--tier", tier.value, "--preview"]);
    if (!preview) return;
    let digest: string;
    try {
      const parsed = JSON.parse(preview);
      digest = parsed.digest;
      if (typeof digest !== "string") throw new Error();
      const bundle = parsed.bundle;
      await showText("Publication preview", [
        summary.trim(), "", `Disclosure tier: ${tier.value}`, `Bundle digest: ${digest}`,
        `Included: ${bundle.chain_heads.length} chain heads, ${bundle.events.length} signed events, ${bundle.prompts.length} prompt commitments, ${bundle.rotations.length} key rotations.`,
        "Events can include milestone claims and evidence references. Local checks do not guarantee that the commons will verify them.",
        "The exact bundle to be published follows:", "", JSON.stringify(bundle, null, 2),
      ].join("\n"));
    }
    catch { void vscode.window.showErrorMessage("Invalid publication preview."); return; }
    const destination = await vscode.window.showInputBox({ title: "Commons URL", prompt: "Review the exact bundle. Local evidence checks do not guarantee public verification.", placeHolder: "https://commons.example" });
    if (!destination?.trim()) return;
    const confirm = await vscode.window.showWarningMessage("Publish the displayed bundle? Its event and evidence data will be public and cannot be retracted.", { modal: true }, "Publish");
    if (confirm !== "Publish") return;
    const out = await speedrun(["publish", "--run", id, "--tier", tier.value, "--to", destination.trim(), "--expect-digest", digest], undefined, 120_000);
    if (out) await showText("Publication receipt", out);
  });

  register("agentspeedrun.pause", () => speedrun(["pause"]));
  register("agentspeedrun.resume", () => speedrun(["resume"]));

  register("agentspeedrun.reset", async () => {
    const ok = await vscode.window.showWarningMessage(
      "Close the current run? An unfinished run is abandoned; the next prompt starts a new one.",
      { modal: true },
      "Close run",
    );
    if (ok) await speedrun(["reset", "--yes"]);
  });

  register("agentspeedrun.watch", () => {
    const terminal = vscode.window.createTerminal({ name: "speedrun watch", cwd: project?.folder.uri });
    terminal.sendText(`${bin()} watch`);
    terminal.show();
  });

  register("agentspeedrun.trust", async () => {
    const out = await speedrun(["init", "--trust"]);
    if (out) void vscode.window.showInformationMessage(out.trim());
  });

  register("agentspeedrun.setupCopilotHooks", async () => {
    const out = await speedrun(["setup", "copilot", "--project"]);
    if (out) {
      void vscode.window.showInformationMessage(
        "agentspeedrun: Copilot agent hooks installed in .github/hooks/. Commit them to time this repo's agent prompts.",
      );
    }
  });

  await detect();

  return {
    statusText: () => (project ? status.text : undefined),
    run: currentRun,
    mcpServerDefinitions,
  };
}

export function deactivate() {}
