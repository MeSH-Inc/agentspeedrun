import { afterAll, beforeAll, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { writeFileSync, chmodSync } from "node:fs";
import { join } from "node:path";
import { ofType, sandbox, SPEEDRUN, type Sandbox } from "../../testkit";
import plugin from "../agentspeedrun";

let sb: Sandbox;
const hooks: Record<string, (event: any) => Promise<void>> = {};
const tools: Record<string, any> = {};
let push: (event: any) => void;
let cleanup: () => void;

beforeAll(async () => {
  sb = await sandbox();
  let resolve: ((value: any) => void) | undefined;
  const queue: any[] = [];
  push = (event) => { if (resolve) { const r = resolve; resolve = undefined; r({ value: event, done: false }); } else queue.push(event); };
  cleanup = await plugin.setup({
    location: { directory: sb.project },
    session: {
      get: async ({ sessionID }: any) => ({ parentID: sessionID.startsWith("child") ? "parent" : undefined }),
      hook: async (name: string, fn: any) => { hooks[name] = fn; },
    },
    tool: { transform: async (fn: any) => fn({ add: (tool: any) => { tools[tool.name] = tool; } }) },
    event: { subscribe: () => ({ [Symbol.asyncIterator]: () => ({ next: () => queue.length ? Promise.resolve({ value: queue.shift(), done: false }) : new Promise((r) => { resolve = r; }) }) }) },
  });
});
afterAll(() => { cleanup?.(); sb?.close(); });

test("registers the CLI's tool schemas unchanged", () => {
  const defs = JSON.parse(execFileSync(SPEEDRUN, ["tool", "--list"], { encoding: "utf8" }));
  expect(Object.keys(tools)).toEqual(defs.map((d: any) => d.name));
  for (const d of defs) {
    expect(tools[d.name].description).toBe(d.description);
    expect(tools[d.name].input).toEqual(d.inputSchema);
  }
});

test("captures human prompt, injects context and ends execution once", async () => {
  await hooks.prompt({ sessionID: "child_1", prompt: { text: "child" } });
  await hooks.prompt({ sessionID: "main", prompt: { text: "build a recipe box" } });
  const [prompt] = ofType(sb.events(), "prompt_submitted");
  expect(prompt.payload.length).toBe("build a recipe box".length);
  expect(prompt.payload.model).toBeNull();
  expect(prompt.payload).not.toHaveProperty("content");
  expect(prompt.sig).toBeTruthy();
  expect(sb.status().prompt_count).toBe(1);
  const context = { sessionID: "main", system: [] as any[] };
  await hooks.context(context);
  expect(context.system[0].text).toContain("started a timed `any-percent` run");
  expect((await tools.speedrun_status.execute({}, { sessionID: "main" })).content).toContain("scaffold-runs");
  expect((await tools.speedrun_propose_milestones.execute({ milestones: [{ title: "Recipes persist" }] }, { sessionID: "main" })).content).toContain("Proposed 1 milestones");
  const after = { sessionID: "main", system: [] as any[] };
  await hooks.context(after);
  expect(after.system).toEqual([]);
  push({ type: "session.execution.succeeded", data: { sessionID: "main" } });
  await Bun.sleep(100);
  push({ type: "session.execution.succeeded", data: { sessionID: "main" } });
  await Bun.sleep(100);
  expect(ofType(sb.events(), "agent_turn_ended")).toHaveLength(1);
  expect(sb.status().open_turns).toBe(0);
});

test("failed and interrupted executions end their turns", async () => {
  for (const type of ["session.execution.failed", "session.execution.interrupted"]) {
    await hooks.prompt({ sessionID: "main", prompt: { text: type } });
    push({ type: "irrelevant" });
    push({ type, data: { sessionID: "main" } });
    await Bun.sleep(100);
    expect(sb.status().open_turns).toBe(0);
  }
  expect(ofType(sb.events(), "agent_turn_ended")).toHaveLength(3);
});

test("tool failures reach the agent and missing CLI does not break prompt admission", async () => {
  await expect(tools.speedrun_claim_milestone.execute({ milestone: "nope" }, { sessionID: "main" })).rejects.toThrow("no milestone");
  const saved = process.env.SPEEDRUN_BIN;
  process.env.SPEEDRUN_BIN = "/nonexistent/speedrun";
  try {
    await hooks.prompt({ sessionID: "main", prompt: { text: "still works?" } });
    expect((await tools.speedrun_status.execute({}, { sessionID: "main" })).content).toContain("not installed");
  } finally { process.env.SPEEDRUN_BIN = saved; }
});

test("deleting a session closes its turn", async () => {
  await hooks.prompt({ sessionID: "deleted", prompt: { text: "another prompt" } });
  push({ type: "session.deleted", data: { sessionID: "deleted" } });
  await Bun.sleep(100);
  expect(sb.status().open_turns).toBe(0);
});

test("a zero exit with a rejected turn end keeps the session pending", async () => {
  await hooks.prompt({ sessionID: "retry", prompt: { text: "retry completion" } });
  const stub = join(sb.project, "reject-ingest");
  writeFileSync(stub, `#!/bin/sh
echo '{"recorded":false,"reason":"not a speedrun project"}'
`);
  chmodSync(stub, 0o755);
  const saved = process.env.SPEEDRUN_BIN;
  process.env.SPEEDRUN_BIN = stub;
  try {
    push({ type: "session.execution.succeeded", data: { sessionID: "retry" } });
    await Bun.sleep(100);
    expect(sb.status().open_turns).toBe(1);
  } finally { process.env.SPEEDRUN_BIN = saved; }
  push({ type: "session.execution.succeeded", data: { sessionID: "retry" } });
  await Bun.sleep(100);
  expect(sb.status().open_turns).toBe(0);
});

test("cleanup does not invent a turn end during hot reload", async () => {
  await hooks.prompt({ sessionID: "unfinished", prompt: { text: "work" } });
  expect(sb.status().open_turns).toBe(1);
  cleanup();
  expect(sb.status().open_turns).toBe(1);
});
