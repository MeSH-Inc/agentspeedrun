// agentspeedrun adapter for opencode (https://opencode.ai/docs/plugins/).
//
// Thin by design: every behavior lives in the `speedrun` CLI. This file maps
// opencode's hooks onto `speedrun ingest` (prompts, turn ends) and
// `speedrun tool` (the milestone tools), and must never break opencode:
// every failure is swallowed and logged only with SPEEDRUN_DEBUG=1.
//
// Installed by `speedrun setup opencode`. Generated from
// plugins/opencode/agentspeedrun.ts in mesh-inc/agentspeedrun.

import { spawn, spawnSync } from "node:child_process";

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

type ToolSpec = { name: string; description: string; inputSchema: object };

async function toolSpecs(cwd: string): Promise<ToolSpec[]> {
  const { code, stdout } = await speedrun(["tool", "--list"], cwd);
  if (code !== 0) return [];
  try { return JSON.parse(stdout); } catch { return []; }
}

// OpenCode v2 supplies these APIs at runtime. There are no SDK imports, so a
// fresh project plugin can load without a package manager or network access.
type SessionPrompt = { sessionID: string; prompt: { text?: string } };
type SessionContext = { sessionID: string; system: { type: string; text: string }[] };
type HostEvent = { type: string; data?: { sessionID?: string } };
type ToolDefinition = { name: string; description: string; input: object; execute: (args: unknown, context: { sessionID: string }) => Promise<{ content: string }> };
type HostContext = {
  location: { directory: string };
  session: {
    get: (input: { sessionID: string }) => Promise<{ parentID?: string; data?: { parentID?: string } }>;
    hook: {
      (name: "prompt", callback: (event: SessionPrompt) => void | Promise<void>): Promise<unknown>;
      (name: "context", callback: (event: SessionContext) => void | Promise<void>): Promise<unknown>;
    };
  };
  tool: { transform: (callback: (editor: { add: (definition: ToolDefinition) => void }) => void) => Promise<unknown> };
  event: { subscribe: (options: { signal: AbortSignal }) => AsyncIterable<HostEvent> };
};

export default {
  id: "agentspeedrun",
  async setup(ctx: HostContext) {
    try {
      const directory: string = ctx.location.directory;
      const busy = new Set<string>();
      const pending = new Map<string, string>();
      const controller = new AbortController();
      function endTurn(sessionID: string) {
        if (!busy.has(sessionID)) return;
        // OpenCode standalone can exit immediately after publishing completion.
        const result = spawnSync(bin(), ["ingest", "turn-end", "--host", HOST, "--session", sessionID], { cwd: directory, timeout: 2_000, encoding: "utf8" });
        try {
          const response = JSON.parse(result.stdout.trim().split("\n").pop() ?? "{}");
          if (result.status === 0 && response.recorded === true) busy.delete(sessionID);
          else debug("turn-end was not recorded", result.error ?? result.status);
        } catch (e) { debug("turn-end response", e); }
      }
      const specs = await toolSpecs(directory);
      await ctx.tool.transform((editor) => {
        for (const spec of specs) editor.add({
          name: spec.name, description: spec.description, input: spec.inputSchema,
          execute: async (args: unknown, context: { sessionID: string }) => {
            const content = await callTool(spec.name, args, directory);
            if (spec.name === "speedrun_propose_milestones") pending.delete(context.sessionID);
            return { content };
          },
        });
      });
      await ctx.session.hook("prompt", async (event: SessionPrompt) => {
        try {
          const session = await ctx.session.get({ sessionID: event.sessionID });
          if (session.parentID || session.data?.parentID) return;
          const text = event.prompt.text;
          if (!text?.trim()) return;
          const res = await ingest(["prompt", "--host", HOST, "--session", event.sessionID], directory, text);
          if (res.recorded) busy.add(event.sessionID);
          if (res.run_started && res.context) pending.set(event.sessionID, res.context);
        } catch (e) { debug("prompt", e); }
      });
      await ctx.session.hook("context", (event: SessionContext) => {
        const context = pending.get(event.sessionID);
        if (context) event.system.push({ type: "text", text: context });
      });
      void (async () => {
        try {
          for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
            const data = event.data;
            if (!data?.sessionID) continue;
            if (["session.execution.succeeded", "session.execution.failed", "session.execution.interrupted", "session.deleted"].includes(event.type)) {
              endTurn(data.sessionID);
              if (event.type === "session.deleted") pending.delete(data.sessionID);
            }
          }
        } catch (e) { if (!controller.signal.aborted) debug("event stream", e); }
      })();
      return () => { controller.abort(); pending.clear(); };
    } catch (e) { debug("setup", e); return () => {}; }
  },
};
