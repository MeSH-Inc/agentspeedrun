import * as assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as vscode from "vscode";
import type { Api } from "../../extension";

const project = process.env.SPEEDRUN_TEST_PROJECT!;
const speedrun = (args: string[], input = "") =>
  execFileSync(process.env.SPEEDRUN_BIN!, args, { cwd: project, input, encoding: "utf8" });

async function until(what: string, cond: () => boolean, ms = 10_000, diagnostics?: () => string) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}${diagnostics ? `: ${diagnostics()}` : ""}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

describe("agentspeedrun in VS Code", () => {
  let api: Api;

  before(async () => {
    const ext = vscode.extensions.getExtension<Api>("mesh-inc.agentspeedrun");
    assert.ok(ext, "extension is installed");
    api = await ext.activate();
  });

  it("reports that a tracked workspace needs an integration", async () => {
    await until("integration status", () => /integration needed/.test(api.statusText() ?? ""));
    assert.match(api.statusText()!, /integration needed/);
  });

  it("offers the recording setup action only when requested", async () => {
    const window = vscode.window as unknown as { showInformationMessage: typeof vscode.window.showInformationMessage };
    const original = window.showInformationMessage;
    let message = "";
    let options: unknown[] = [];
    try {
      window.showInformationMessage = (async (text: string, ...items: unknown[]) => {
        message = text;
        options = items;
        return undefined;
      }) as typeof vscode.window.showInformationMessage;
      await vscode.commands.executeCommand("agentspeedrun.capture");
      assert.match(message, /integration/i);
      assert.ok(options.includes("Open setup"));
    } finally {
      window.showInformationMessage = original;
    }
  });

  it("offers CLI-directed recovery without running it automatically", async () => {
    const directory = mkdtempSync(join(tmpdir(), "speedrun-readiness-"));
    const binary = join(directory, "speedrun");
    const config = vscode.workspace.getConfiguration("agentspeedrun");
    const previous = config.inspect<string>("speedrunPath")?.workspaceValue;
    const window = vscode.window as unknown as { showInformationMessage: typeof vscode.window.showInformationMessage };
    const original = window.showInformationMessage;
    const base = { version: 1, project: "trusted", daemon: "online", integrations: [], integration_state: "missing", selected_integration: null, recording: "unknown", last_activity: null, last_host: null, queue_scope: "device", queued: 0, rejected: 0, write_failures: 0 };
    try {
      await config.update("speedrunPath", binary, vscode.ConfigurationTarget.Workspace);
      for (const [id, expected] of [["setup", "Open setup"], ["flush", "Start service and deliver queued requests"], ["doctor", "Check diagnostics"], ["inspect_rejected", "Inspect rejected requests"], ["send_prompt", undefined], ["none", undefined]] as const) {
        const payload = JSON.stringify({ ...base, next_action: `Guidance for ${id}`, next_action_id: id });
        writeFileSync(binary, `#!/bin/sh\nprintf '%s\\n' '${payload}'\n`);
        chmodSync(binary, 0o755);
        let choices: unknown[] = [];
        window.showInformationMessage = (async (_message: string, ...items: unknown[]) => { choices = items; return undefined; }) as typeof vscode.window.showInformationMessage;
        await vscode.commands.executeCommand("agentspeedrun.capture");
        assert.deepEqual(choices, expected ? [expected] : [], id);
      }
      for (const [id, label, command] of [["doctor", "Check diagnostics", "doctor"], ["inspect_rejected", "Inspect rejected requests", "outbox rejected"]] as const) {
        const log = join(directory, "calls");
        const payload = JSON.stringify({ ...base, next_action: `Guidance for ${id}`, next_action_id: id });
        writeFileSync(binary, `#!/bin/sh\nif [ "$1" = readiness ]; then printf '%s\\n' '${payload}'; else printf '%s\\n' "$*" > '${log}'; printf 'Details for %s\\n' "$*"; fi\n`);
        chmodSync(binary, 0o755);
        window.showInformationMessage = (async () => label) as typeof vscode.window.showInformationMessage;
        await vscode.commands.executeCommand("agentspeedrun.capture");
        assert.equal(readFileSync(log, "utf8").trim(), command);
        assert.match(vscode.window.activeTextEditor?.document.getText() ?? "", new RegExp(`Details for ${command}`));
      }
    } finally {
      window.showInformationMessage = original;
      await config.update("speedrunPath", previous, vscode.ConfigurationTarget.Workspace);
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("contributes its commands", async () => {
    const all = await vscode.commands.getCommands(true);
    for (const id of ["init", "milestone", "split", "add", "accept", "reject", "pause", "resume", "reset", "watch", "setupCopilotHooks", "confirm", "invalidate", "capture", "history", "publish", "verification", "localTest"]) {
      assert.ok(all.includes(`agentspeedrun.${id}`), id);
    }
  });

  it("shows the run from the live stream and ticks locally", async () => {
    speedrun(["ingest", "prompt", "--host", "test", "--session", "s1"], "build a recipe box");
    await until("run in status bar", () => /Any% 0:00:\d\d · 0\/2/.test(api.statusText() ?? ""));
    const first = api.statusText();
    await until("timer tick", () => api.statusText() !== first, 4000);
  });

  it("splits through the command and reflects it", async function () {
    this.timeout(30_000);
    await vscode.commands.executeCommand("agentspeedrun.split", "scaffold-runs", "");
    await until("split counted", () => api.run()?.milestones.find((m) => m.key === "scaffold-runs")?.counts === true);
    await until("progress in status", () => / · 1\/2/.test(api.statusText() ?? ""), 15_000, () => api.statusText() ?? "no status");
  });

  it("adds a user milestone", async () => {
    await vscode.commands.executeCommand("agentspeedrun.add", "Recipes persist");
    await until("milestone listed", () => api.run()?.milestones.some((m) => m.title === "Recipes persist") === true);
  });

  it("opens a readable history entry for the captured run", async () => {
    const runId = api.run()?.run_id;
    assert.ok(runId);
    await vscode.commands.executeCommand("agentspeedrun.history", runId);
    assert.match(vscode.window.activeTextEditor?.document.getText() ?? "", /run .* —/);
    assert.match(vscode.window.activeTextEditor?.document.getText() ?? "", /scaffold-runs/);
  });

  it("records a milestone through the contextual action", async () => {
    const window = vscode.window as unknown as { showQuickPick: typeof vscode.window.showQuickPick };
    const original = window.showQuickPick;
    try {
      window.showQuickPick = (async (items: unknown) => (items as Array<{ action?: { id: string } }>).find((item) => item.action?.id === "split")) as typeof vscode.window.showQuickPick;
      await vscode.commands.executeCommand("agentspeedrun.milestone", "happy-path-demo");
      await until("guided milestone counted", () => api.run()?.milestones.find((m) => m.key === "happy-path-demo")?.counts === true);
    } finally {
      window.showQuickPick = original;
    }
  });

  it("offers the MCP server for this project", () => {
    const [def] = api.mcpServerDefinitions();
    assert.equal(def.label, "agentspeedrun");
    assert.deepEqual(def.args, ["mcp", "--project-dir", project]);
  });
});
