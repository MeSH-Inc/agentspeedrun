// agentspeedrun extension for pi (badlogic/pi-mono) and oh-my-pi (can1357/oh-my-pi).
//
// Thin by design: every behavior lives in the `speedrun` CLI. This file maps
// pi's events onto `speedrun ingest` (prompts, turn ends) and `speedrun tool`
// (the milestone tools), and must never break the agent: every failure is
// swallowed and logged only with SPEEDRUN_DEBUG=1.
//
// Installed by `speedrun setup pi` / `speedrun setup omp`, or `pi install` of
// the repository. Source: plugins/pi/agentspeedrun.ts in mesh-inc/agentspeedrun.

import { execFileSync, spawn } from "node:child_process";
import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent";
import { type TSchema, Type } from "typebox";

/** Read per call so tests and users can point at a specific binary. */
const bin = () => process.env.SPEEDRUN_BIN ?? "speedrun";
const TIMEOUT_MS = 10_000;
const STATUS_KEY = "agentspeedrun";

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

function text(t: string) {
  return { content: [{ type: "text" as const, text: t }], details: {} };
}

async function callTool(name: string, args: unknown, cwd: string) {
  const { code, stdout } = await speedrun(["tool", name, "--args", JSON.stringify(args)], cwd);
  if (code === -1) return text("agentspeedrun: the `speedrun` CLI is not installed or not on PATH.");
  if (code !== 0) throw new Error(stdout.trim() || `speedrun tool ${name} failed`);
  return text(stdout.trim());
}

/** oh-my-pi's context carries the agent identity; pi's does not. */
function host(ctx: ExtensionContext): string {
  return "agent" in ctx ? "omp" : "pi";
}

/** oh-my-pi rebinds extensions into every subagent; only the main agent's prompts are the human's. */
function isSubagent(ctx: ExtensionContext): boolean {
  return (ctx as { agent?: { kind?: string } }).agent?.kind === "sub";
}

function sessionId(ctx: ExtensionContext): string {
  try {
    return ctx.sessionManager.getSessionId() || "unknown";
  } catch {
    return "unknown";
  }
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

/** A TypeBox schema for one JSON Schema node. A type this does not know
 * throws, so a new kind of field in the CLI's definitions fails the drift
 * test instead of reaching the agent as something else. */
function typeboxOf(s: JsonSchema): TSchema {
  const options = s.description ? { description: s.description } : {};
  if (s.type === "string") return Type.String(options);
  if (s.type === "array") {
    const bounds = {
      ...(s.minItems !== undefined ? { minItems: s.minItems } : {}),
      ...(s.maxItems !== undefined ? { maxItems: s.maxItems } : {}),
    };
    return Type.Array(typeboxOf(s.items ?? {}), { ...options, ...bounds });
  }
  if (s.type === "object") {
    const required = new Set(s.required ?? []);
    const properties = Object.fromEntries(
      Object.entries(s.properties ?? {}).map(([key, value]) => {
        const t = typeboxOf(value);
        return [key, required.has(key) ? t : Type.Optional(t)];
      }),
    );
    return Type.Object(properties, options);
  }
  throw new Error(`unsupported schema type ${s.type}`);
}

/** The milestone tools, as `speedrun tool --list` defines them; none when the
 * CLI cannot say, since a tool could only report that it is missing. pi
 * registers tools while the extension loads, so this one call is synchronous. */
function toolSpecs(): ToolSpec[] {
  try {
    return JSON.parse(execFileSync(bin(), ["tool", "--list"], { encoding: "utf8", timeout: TIMEOUT_MS }));
  } catch (e) {
    debug("no tool list from speedrun", e);
    return [];
  }
}

/** Labels pi shows for the tools; a tool without one shows its name. */
const LABELS: Record<string, string> = {
  speedrun_status: "Speedrun status",
  speedrun_propose_milestones: "Propose milestones",
  speedrun_claim_milestone: "Claim milestone",
};

export default function agentspeedrun(pi: ExtensionAPI) {
  // Sessions with an agent turn in progress, to send exactly one turn-end per turn.
  const busy = new Set<string>();

  async function refreshStatus(ctx: ExtensionContext) {
    if (!ctx.hasUI || isSubagent(ctx)) return;
    const { code, stdout } = await speedrun(["status", "--line"], ctx.cwd);
    ctx.ui.setStatus(STATUS_KEY, code === 0 && stdout.trim() ? stdout.trim() : undefined);
  }

  async function endTurn(ctx: ExtensionContext) {
    const session = sessionId(ctx);
    if (!busy.delete(session)) return;
    await ingest(["turn-end", "--host", host(ctx), "--session", session], ctx.cwd);
  }

  pi.on("session_start", async (_event, ctx) => {
    try {
      await refreshStatus(ctx);
    } catch (e) {
      debug("session_start", e);
    }
  });

  pi.on("before_agent_start", async (event, ctx) => {
    try {
      // oh-my-pi fires this with reason "revive" and an empty prompt for parked subagents.
      const reason = (event as { reason?: string }).reason;
      if (isSubagent(ctx) || reason === "revive" || !event.prompt?.trim()) return;
      const session = sessionId(ctx);
      const args = ["prompt", "--host", host(ctx), "--session", session];
      if (ctx.model?.id) args.push("--model", `${ctx.model.provider}/${ctx.model.id}`);
      const res = await ingest(args, ctx.cwd, event.prompt);
      if (res.recorded) busy.add(session);
      await refreshStatus(ctx);
      if (res.run_started && res.context) {
        return { message: { customType: "agentspeedrun", content: res.context, display: false } };
      }
    } catch (e) {
      debug("before_agent_start", e);
    }
  });

  pi.on("agent_end", async (_event, ctx) => {
    try {
      if (isSubagent(ctx)) return;
      await endTurn(ctx);
      await refreshStatus(ctx);
    } catch (e) {
      debug("agent_end", e);
    }
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    try {
      if (!isSubagent(ctx)) await endTurn(ctx);
    } catch (e) {
      debug("session_shutdown", e);
    }
  });

  for (const spec of toolSpecs()) {
    pi.registerTool({
      name: spec.name,
      label: LABELS[spec.name] ?? spec.name,
      description: spec.description,
      parameters: typeboxOf(spec.inputSchema),
      execute: async (_id, params, _signal, _onUpdate, ctx) => {
        const result = await callTool(spec.name, params, ctx.cwd);
        // A claim changes the run's progress, which the status line shows.
        if (spec.name === "speedrun_claim_milestone") await refreshStatus(ctx);
        return result;
      },
    });
  }
}
