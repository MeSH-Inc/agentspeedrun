// agentspeedrun adapter for opencode (https://opencode.ai/docs/plugins/).
//
// Thin by design: every behavior lives in the `speedrun` CLI. This file maps
// opencode's hooks onto `speedrun ingest` (prompts, turn ends) and
// `speedrun tool` (the milestone tools), and must never break opencode:
// every failure is swallowed and logged only with SPEEDRUN_DEBUG=1.
//
// Installed by `speedrun setup opencode`. Generated from
// plugins/opencode/agentspeedrun.ts in mesh-inc/agentspeedrun.

import { spawn } from "node:child_process";
import { type Plugin, type ToolDefinition, tool } from "@opencode-ai/plugin";
import type { ZodType } from "zod";

/** Read per call so tests and users can point at a specific binary. */
const bin = () => process.env.SPEEDRUN_BIN ?? "speedrun";
const HOST = "opencode";
const TIMEOUT_MS = 10_000;

type RunResult = { code: number | null; stdout: string };

/** Run the speedrun CLI. Never rejects. */
function speedrun(args: string[], cwd: string, input?: string): Promise<RunResult> {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(bin(), args, { cwd, stdio: ["pipe", "pipe", "pipe"], timeout: TIMEOUT_MS });
    } catch (e) {
      debug("spawn failed", e);
      return resolve({ code: -1, stdout: "" });
    }
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    child.on("error", (e) => {
      debug("speedrun not runnable (is it on PATH?)", e);
      resolve({ code: -1, stdout: "" });
    });
    child.on("close", (code) => {
      if (code !== 0) debug(`speedrun ${args[0]} exited ${code}`, stderr);
      resolve({ code, stdout });
    });
    child.stdin.end(input ?? "");
  });
}

function debug(...msg: unknown[]) {
  if (process.env.SPEEDRUN_DEBUG) console.error("[agentspeedrun]", ...msg);
}

type IngestResult = { recorded?: boolean; run_started?: boolean; context?: string | null };

async function ingest(args: string[], cwd: string, input?: string): Promise<IngestResult> {
  const { stdout } = await speedrun(["ingest", ...args], cwd, input);
  try {
    return JSON.parse(stdout.trim().split("\n").pop() ?? "{}");
  } catch {
    return {};
  }
}

async function callTool(name: string, args: unknown, cwd: string): Promise<string> {
  const { code, stdout } = await speedrun(["tool", name, "--args", JSON.stringify(args)], cwd);
  if (code === -1) return "agentspeedrun: the `speedrun` CLI is not installed or not on PATH.";
  if (code !== 0) throw new Error(stdout.trim() || `speedrun tool ${name} failed`);
  return stdout.trim();
}

/** The subset of JSON Schema `speedrun tool --list` uses. */
type JsonSchema = {
  type?: string;
  description?: string;
  properties?: Record<string, JsonSchema>;
  required?: string[];
  items?: JsonSchema;
  minItems?: number;
  maxItems?: number;
};

type ToolSpec = { name: string; description: string; inputSchema: JsonSchema };

type Schema = ZodType;

/** A zod schema for one JSON Schema node. A type this does not know throws,
 * so a new kind of field in the CLI's definitions fails the drift test
 * instead of reaching the agent as something else. */
function zodOf(s: JsonSchema): Schema {
  let z: Schema;
  if (s.type === "string") {
    z = tool.schema.string();
  } else if (s.type === "array") {
    let a = tool.schema.array(zodOf(s.items ?? {}));
    if (s.minItems !== undefined) a = a.min(s.minItems);
    if (s.maxItems !== undefined) a = a.max(s.maxItems);
    z = a;
  } else if (s.type === "object") {
    z = tool.schema.object(shapeOf(s));
  } else {
    throw new Error(`unsupported schema type ${s.type}`);
  }
  return s.description ? z.describe(s.description) : z;
}

/** The properties of an object schema, those not required made optional. */
function shapeOf(s: JsonSchema) {
  const required = new Set(s.required ?? []);
  return Object.fromEntries(
    Object.entries(s.properties ?? {}).map(([key, value]) => {
      const z = zodOf(value);
      return [key, required.has(key) ? z : z.optional()];
    }),
  );
}

/** The milestone tools, as `speedrun tool --list` defines them; none when the
 * CLI cannot say, since a tool could only report that it is missing. */
async function toolSpecs(cwd: string): Promise<ToolSpec[]> {
  const { code, stdout } = await speedrun(["tool", "--list"], cwd);
  if (code !== 0) return [];
  try {
    return JSON.parse(stdout);
  } catch (e) {
    debug("unreadable tool list", e);
    return [];
  }
}

/** One opencode tool per CLI definition, each calling `speedrun tool`. */
export async function milestoneTools(cwd: string): Promise<Record<string, ToolDefinition>> {
  const tools: Record<string, ToolDefinition> = {};
  for (const spec of await toolSpecs(cwd)) {
    tools[spec.name] = tool({
      description: spec.description,
      args: shapeOf(spec.inputSchema),
      execute: async (args, ctx) => callTool(spec.name, args, ctx.directory),
    });
  }
  return tools;
}

export const AgentSpeedrun: Plugin = async ({ client, directory }) => {
  // Sessions with an agent turn in progress, to send exactly one turn-end per turn.
  const busy = new Set<string>();
  // Run-start context to show the agent, per session, until it proposes milestones.
  const pendingContext = new Map<string, string>();
  // Subagent (child) sessions: their prompts come from the agent, not the human.
  const isChild = new Map<string, boolean>();

  async function childSession(sessionID: string): Promise<boolean> {
    const known = isChild.get(sessionID);
    if (known !== undefined) return known;
    try {
      const res = await client.session.get({ path: { id: sessionID } });
      const child = Boolean(res.data?.parentID);
      isChild.set(sessionID, child);
      return child;
    } catch (e) {
      debug("session lookup failed", e);
      return false;
    }
  }

  async function endTurn(sessionID: string) {
    if (!busy.delete(sessionID)) return;
    await ingest(["turn-end", "--host", HOST, "--session", sessionID], directory);
  }

  return {
    "chat.message": async (input, output) => {
      try {
        if (await childSession(input.sessionID)) return;
        const text = output.parts
          .filter((p) => p.type === "text" && !p.synthetic && !p.ignored)
          .map((p) => (p as { text: string }).text)
          .join("\n\n");
        if (!text.trim()) return;
        const model = input.model ? `${input.model.providerID}/${input.model.modelID}` : undefined;
        const args = ["prompt", "--host", HOST, "--session", input.sessionID];
        if (model) args.push("--model", model);
        const res = await ingest(args, directory, text);
        if (res.recorded) busy.add(input.sessionID);
        if (res.run_started && res.context) pendingContext.set(input.sessionID, res.context);
      } catch (e) {
        debug("chat.message", e);
      }
    },

    "experimental.chat.system.transform": async (input, output) => {
      const context = input.sessionID ? pendingContext.get(input.sessionID) : undefined;
      if (context) output.system.push(context);
    },

    "tool.execute.after": async (input) => {
      if (input.tool === "speedrun_propose_milestones") pendingContext.delete(input.sessionID);
    },

    event: async ({ event }) => {
      try {
        if (event.type === "session.created") {
          isChild.set(event.properties.info.id, Boolean(event.properties.info.parentID));
        } else if (event.type === "session.idle") {
          await endTurn(event.properties.sessionID);
        } else if (event.type === "session.status" && event.properties.status.type === "idle") {
          await endTurn(event.properties.sessionID);
        } else if (event.type === "session.deleted") {
          await endTurn(event.properties.info.id);
          pendingContext.delete(event.properties.info.id);
          isChild.delete(event.properties.info.id);
        }
      } catch (e) {
        debug("event", e);
      }
    },

    tool: await milestoneTools(directory),
  };
};
