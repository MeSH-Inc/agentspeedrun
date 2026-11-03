import { afterAll, beforeAll, expect, test } from "bun:test";
import { passingCi, REPOSITORY, sandbox, type Sandbox } from "../../testkit";

// V0.2 W5.a: an agent in pi reaches evidence_verified on tests-green, with
// GitHub's API answered by a stand-in that reports the run as passed.

let sb: Sandbox;
let ci: { url: string; close: () => void };
const handlers = new Map<string, Function>();
const tools = new Map<string, any>();

const pi = {
  on: (event: string, handler: Function) => handlers.set(event, handler),
  registerTool: (def: any) => tools.set(def.name, def),
};

const ctx = () => ({
  cwd: sb.project,
  hasUI: false,
  sessionManager: { getSessionId: () => "pi-ci" },
  model: { id: "claude-x", provider: "anthropic" },
});

beforeAll(async () => {
  ci = passingCi(() => sb.head());
  sb = await sandbox({ category: "hundred-percent", daemonEnv: { SPEEDRUN_GITHUB_API: ci.url } });
  const { default: agentspeedrun } = await import("../agentspeedrun");
  agentspeedrun(pi as any);
});

afterAll(() => {
  sb?.close();
  ci?.close();
});

test("a passing CI run verifies tests-green", async () => {
  await handlers.get("before_agent_start")!({ prompt: "add tests for the recipe box" }, ctx());
  sb.commit("cover recipes with tests");
  const claimed = await tools
    .get("speedrun_claim_milestone")
    .execute(
      "call-1",
      { milestone: "tests-green", ci_run: `https://github.com/${REPOSITORY}/actions/runs/42` },
      undefined,
      undefined,
      ctx(),
    );
  expect(claimed.content[0].text).toContain("Evidence verified in Local scope (ci)");
  const milestone = sb.status().milestones.find((m: any) => m.key === "tests-green");
  expect(milestone.trust).toBe("evidence_verified");
});
