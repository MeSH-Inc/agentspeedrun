import { afterAll, beforeAll, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { ofType, sandbox, SPEEDRUN, type Sandbox } from "../../testkit";

let sb: Sandbox;
const handlers = new Map<string, Function>();
const tools = new Map<string, any>();
const statuses: (string | undefined)[] = [];

const pi = {
  on: (event: string, handler: Function) => handlers.set(event, handler),
  registerTool: (def: any) => tools.set(def.name, def),
};

/** A pi context, or an oh-my-pi one when `agent` is given. */
const ctx = (session: string, agent?: { kind: "main" | "sub" }) => ({
  cwd: sb.project,
  hasUI: true,
  ui: { setStatus: (_key: string, value?: string) => statuses.push(value) },
  sessionManager: { getSessionId: () => session },
  model: { id: "claude-x", provider: "anthropic" },
  ...(agent ? { agent } : {}),
});

const fire = (event: string, payload: any, c: any) => handlers.get(event)!(payload, c);

beforeAll(async () => {
  sb = await sandbox();
  const { default: agentspeedrun } = await import("../agentspeedrun");
  agentspeedrun(pi as any);
});

afterAll(() => sb?.close());

test("registers the three milestone tools", () => {
  expect([...tools.keys()]).toEqual(["speedrun_status", "speedrun_propose_milestones", "speedrun_claim_milestone"]);
});

/** What an agent sees of a JSON Schema: types, fields, which are required,
 * descriptions and array bounds. */
function visible(s: any): any {
  if (!s || typeof s !== "object") return s;
  return {
    type: s.type,
    description: s.description,
    properties: s.properties
      ? Object.fromEntries(Object.entries(s.properties).map(([k, v]) => [k, visible(v)]))
      : undefined,
    required: s.type === "object" ? [...(s.required ?? [])].sort() : undefined,
    items: visible(s.items),
    minItems: s.minItems,
    maxItems: s.maxItems,
  };
}

test("every tool and field the CLI defines reaches the agent", () => {
  const defs = JSON.parse(execFileSync(SPEEDRUN, ["tool", "--list"], { encoding: "utf8" }));
  expect([...tools.keys()]).toEqual(defs.map((d: any) => d.name));
  for (const d of defs) {
    const registered = tools.get(d.name);
    expect(registered.description).toBe(d.description);
    // TypeBox schemas are JSON Schema; JSON drops TypeBox's own markers.
    expect(visible(JSON.parse(JSON.stringify(registered.parameters)))).toEqual(visible(d.inputSchema));
  }
});

test("first prompt starts a run and injects context", async () => {
  const c = ctx("pi-1");
  await fire("session_start", { reason: "startup" }, c);
  const result = await fire("before_agent_start", { prompt: "build a recipe box", systemPrompt: "" }, c);
  expect(result.message.content).toContain("started a timed `any-percent` run");
  expect(result.message.display).toBe(false);
  const run = sb.status();
  expect(run.prompt_count).toBe(1);
  expect(run.hosts).toEqual(["pi"]);
  expect(ofType(sb.events(), "prompt_submitted")[0].payload.model).toBe("anthropic/claude-x");
  expect(statuses.at(-1)).toMatch(/^⏱ any-percent 0\/2 · /);

  const again = await fire("before_agent_start", { prompt: "and add tags", systemPrompt: "" }, c);
  expect(again).toBeUndefined();
});

test("agent_end ends the turn once; shutdown does not double-end", async () => {
  const c = ctx("pi-1");
  await fire("agent_end", { messages: [] }, c);
  await fire("session_shutdown", { reason: "quit" }, c);
  expect(ofType(sb.events(), "agent_turn_ended")).toHaveLength(1);
});

test("oh-my-pi: main agent recorded as omp; subagents and revives ignored", async () => {
  await fire("before_agent_start", { prompt: "omp prompt", systemPrompt: [] }, ctx("omp-1", { kind: "main" }));
  await fire("before_agent_start", { prompt: "sub prompt", systemPrompt: [] }, ctx("omp-sub", { kind: "sub" }));
  await fire("before_agent_start", { prompt: "", reason: "revive", systemPrompt: [] }, ctx("omp-1", { kind: "main" }));
  await fire("agent_end", { messages: [] }, ctx("omp-sub", { kind: "sub" }));
  const run = sb.status();
  expect(run.prompt_count).toBe(3);
  expect(run.hosts).toEqual(["omp", "pi"]);
  expect(ofType(sb.events(), "agent_turn_ended")).toHaveLength(1);
});

test("milestone tools go through the CLI", async () => {
  const c = ctx("pi-1");
  const exec = (name: string, params: any) => tools.get(name).execute("call-1", params, undefined, undefined, c);
  sb.commit("scaffold");
  const claimed = await exec("speedrun_claim_milestone", { milestone: "scaffold-runs", commit: "HEAD" });
  expect(claimed.content[0].text).toContain("this milestone requires a semantic claim; attached evidence alone cannot verify it");
  // An agent's claim on a self_declared milestone counts only once the user confirms it (D6).
  expect(claimed.content[0].text).toContain("waits for the user to confirm it");
  expect(statuses.at(-1)).toMatch(/^⏱ any-percent 0\/2 · /);
  sb.cli("confirm", "scaffold-runs");
  expect(sb.cli("status", "--line")).toMatch(/^⏱ any-percent 1\/2 · /);
  await expect(exec("speedrun_claim_milestone", { milestone: "nope" })).rejects.toThrow("no milestone");
});

test("a missing CLI never breaks pi", async () => {
  const saved = process.env.SPEEDRUN_BIN;
  process.env.SPEEDRUN_BIN = "/nonexistent/speedrun";
  try {
    const c = ctx("pi-2");
    expect(await fire("before_agent_start", { prompt: "hi", systemPrompt: "" }, c)).toBeUndefined();
    await fire("agent_end", { messages: [] }, c);
    const res = await tools.get("speedrun_status").execute("x", {}, undefined, undefined, c);
    expect(res.content[0].text).toContain("not installed");
  } finally {
    process.env.SPEEDRUN_BIN = saved;
  }
});
