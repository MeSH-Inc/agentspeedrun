// agentspeedrun for VS Code: a live timer in the status bar, a splits view,
// commands for the speedrun CLI, and the milestone tools for VS Code's agent
// via MCP. Prompt capture for Copilot's agent comes from its hooks
// (`speedrun setup copilot`), not from this extension: VS Code has no API to
// observe chat prompts. Source: plugins/vscode in mesh-inc/agentspeedrun.

import * as vscode from "vscode";
import { Cli, PROJECT_FILE, projectId } from "./cli";
import { fmtDelta, fmtMs, type LiveMilestone, type LiveRun, LiveClient, type LiveSnapshot, statusText } from "./live";

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
  let warnedMissing = false;

  const currentRun = () => snapshot?.runs.find((r) => r.project_id === project?.id);
  const asOf = () => (snapshot ? new Date(Date.parse(snapshot.as_of)) : new Date());
  // Tick from the local clock, shifted so that skew between us and the daemon cancels out.
  const now = () => new Date(asOf().getTime() + (Date.now() - receivedAt.getTime()));

  // --- status bar ---------------------------------------------------------
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  status.command = "agentspeedrun.splits.focus";
  context.subscriptions.push(status);

  const render = () => {
    const run = currentRun();
    if (!project) return status.hide();
    if (!run) {
      status.text = "$(watch) Speedrun: waiting for first prompt";
      status.tooltip = "The run's timer starts with the first agent prompt in this project.";
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
      ].join("\n"),
    );
    status.show();
  };
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
        m.state === "proposed" ? "proposed by agent" : m.counts ? `${split}${delta}` : m.trust ? `needs ${m.required_trust}` : "";
      item.tooltip = `${m.key ?? m.milestone_id} · ${m.origin} · trust ${m.trust ?? "none"} (needs ${m.required_trust})`;
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
      snapshot = snap;
      receivedAt = new Date();
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
  async function speedrun(args: string[], cwd = project?.folder.uri.fsPath): Promise<string | undefined> {
    if (!cwd) {
      void vscode.window.showWarningMessage("agentspeedrun: open a folder first.");
      return undefined;
    }
    const r = await cli.run(args, cwd);
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

  register("agentspeedrun.split", async (arg?: unknown, commit?: string) => {
    const id = await chooseMilestone(arg, (m) => (m.state === "accepted" || m.state === "reached") && !m.counts, "Split");
    if (!id) return;
    if (commit === undefined) {
      const pick = await vscode.window.showQuickPick(
        [
          { label: "Attach HEAD commit", detail: "The daemon verifies it; needed for evidence_verified milestones.", rev: "HEAD" },
          { label: "No evidence", detail: "Counts only for milestones that accept self_declared.", rev: "" },
        ],
        { title: "Evidence" },
      );
      if (!pick) return;
      commit = pick.rev;
    }
    await speedrun(["split", id, ...(commit ? ["--commit", commit] : [])]);
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
