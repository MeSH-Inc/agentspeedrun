import * as assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as vscode from "vscode";
import type { Api } from "../../extension";

const project = process.env.SPEEDRUN_TEST_PROJECT!;
const speedrun = (args: string[], input = "") =>
  execFileSync(process.env.SPEEDRUN_BIN!, args, { cwd: project, input, encoding: "utf8" });

async function until(what: string, cond: () => boolean, ms = 10_000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
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

  it("activates for a tracked workspace and waits for the first prompt", async () => {
    await until("status", () => api.statusText() !== undefined);
    assert.match(api.statusText()!, /waiting for first prompt/);
  });

  it("contributes its commands", async () => {
    const all = await vscode.commands.getCommands(true);
    for (const id of ["init", "split", "add", "accept", "reject", "pause", "resume", "reset", "watch", "setupCopilotHooks"]) {
      assert.ok(all.includes(`agentspeedrun.${id}`), id);
    }
  });

  it("shows the run from the live stream and ticks locally", async () => {
    speedrun(["ingest", "prompt", "--host", "test", "--session", "s1"], "build a recipe box");
    await until("run in status bar", () => /Any% 0:00:\d\d · 0\/2/.test(api.statusText() ?? ""));
    const first = api.statusText();
    await until("timer tick", () => api.statusText() !== first, 4000);
  });

  it("splits through the command and reflects it", async () => {
    await vscode.commands.executeCommand("agentspeedrun.split", "scaffold-runs", "");
    await until("split counted", () => api.run()?.milestones.find((m) => m.key === "scaffold-runs")?.counts === true);
    await until("progress in status", () => / · 1\/2/.test(api.statusText() ?? ""));
  });

  it("adds a user milestone", async () => {
    await vscode.commands.executeCommand("agentspeedrun.add", "Recipes persist");
    await until("milestone listed", () => api.run()?.milestones.some((m) => m.title === "Recipes persist") === true);
  });

  it("offers the MCP server for this project", () => {
    const [def] = api.mcpServerDefinitions();
    assert.equal(def.label, "agentspeedrun");
    assert.deepEqual(def.args, ["mcp", "--project-dir", project]);
  });
});
