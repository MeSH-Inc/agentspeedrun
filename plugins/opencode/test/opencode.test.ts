import { afterAll, beforeAll, expect, test } from "bun:test";
import { ofType, sandbox, type Sandbox } from "../../testkit";

let sb: Sandbox;
let hooks: any;

// Sessions whose id starts with "child" are subagents of "ses_main".
const client = {
  session: {
    get: async ({ path }: { path: { id: string } }) => ({
      data: { id: path.id, parentID: path.id.startsWith("child") ? "ses_main" : undefined },
    }),
  },
};

const message = (sessionID: string, text: string) => [
  { sessionID, model: { providerID: "anthropic", modelID: "claude-x" } },
  {
    message: {},
    parts: [
      { type: "text", text },
      { type: "text", text: "<file contents injected by opencode>", synthetic: true },
    ],
  },
];

const system = async (sessionID: string) => {
  const out = { system: [] as string[] };
  await hooks["experimental.chat.system.transform"]({ sessionID, model: {} }, out);
  return out.system;
};

const idle = (sessionID: string) => [
  { event: { type: "session.status", properties: { sessionID, status: { type: "idle" } } } },
  { event: { type: "session.idle", properties: { sessionID } } },
];

beforeAll(async () => {
  sb = await sandbox();
  const { AgentSpeedrun } = await import("../agentspeedrun");
  hooks = await AgentSpeedrun({ client, directory: sb.project, worktree: sb.project } as any);
});

afterAll(() => sb?.close());

test("first prompt starts a run and tells the agent about it", async () => {
  await hooks["chat.message"](...message("ses_main", "build a recipe box"));
  const run = sb.status();
  expect(run.prompt_count).toBe(1);
  expect(run.open_turns).toBe(1);
  expect(run.hosts).toEqual(["opencode"]);
  const [prompt] = ofType(sb.events(), "prompt_submitted");
  expect(prompt.payload.content).toBe("build a recipe box");
  expect(prompt.payload.model).toBe("anthropic/claude-x");
  expect((await system("ses_main")).join()).toContain("started a timed `any-percent` run");
  expect(await system("ses_other")).toEqual([]);
});

test("subagent prompts are not recorded as human prompts", async () => {
  await hooks["chat.message"](...message("child_1", "explore the codebase"));
  expect(sb.status().prompt_count).toBe(1);
});

test("going idle ends the turn exactly once", async () => {
  for (const e of idle("ses_main")) await hooks.event(e);
  expect(ofType(sb.events(), "agent_turn_ended")).toHaveLength(1);
  expect(sb.status().open_turns).toBe(0);
  for (const e of idle("child_1")) await hooks.event(e);
  expect(ofType(sb.events(), "agent_turn_ended")).toHaveLength(1);
});

test("milestone tools go through the CLI", async () => {
  const ctx = { directory: sb.project };
  const proposed = await hooks.tool.speedrun_propose_milestones.execute(
    { milestones: [{ title: "Recipes persist" }] },
    ctx,
  );
  expect(proposed).toContain("Proposed 1 milestones");
  await hooks["tool.execute.after"]({ tool: "speedrun_propose_milestones", sessionID: "ses_main" }, {});
  expect(await system("ses_main")).toEqual([]);

  const claimed = await hooks.tool.speedrun_claim_milestone.execute({ milestone: "scaffold-runs", commit: "HEAD" }, ctx);
  expect(claimed).toContain("it counts");
  expect(await hooks.tool.speedrun_status.execute({}, ctx)).toContain("scaffold-runs");
  await expect(hooks.tool.speedrun_claim_milestone.execute({ milestone: "nope" }, ctx)).rejects.toThrow("no milestone");
});

test("a missing CLI never breaks opencode", async () => {
  const saved = process.env.SPEEDRUN_BIN;
  process.env.SPEEDRUN_BIN = "/nonexistent/speedrun";
  try {
    await hooks["chat.message"](...message("ses_main", "still works?"));
    for (const e of idle("ses_main")) await hooks.event(e);
    expect(await hooks.tool.speedrun_status.execute({}, { directory: sb.project })).toContain("not installed");
  } finally {
    process.env.SPEEDRUN_BIN = saved;
  }
});
