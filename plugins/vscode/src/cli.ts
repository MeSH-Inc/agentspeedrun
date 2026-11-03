// Calls into the `speedrun` CLI. All behavior lives there; this extension
// only presents it. No VS Code imports, so it is testable with Bun.

import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { Connection } from "./live";

export type Readiness = {
  version: number; project: "untracked" | "untrusted" | "trusted";
  daemon: "online" | "offline" | "wrong_data_dir";
  integrations: string[]; selected_integration: [string, string] | null; recording: "unknown" | "awaiting_prompt" | "captured";
  last_activity: string | null; last_host: string | null; queue_scope: "device"; queued: number; rejected: number; write_failures: number; next_action: string;
};

/** Reject incompatible or partial results instead of presenting them as trusted state. */
export function parseReadiness(value: unknown): Readiness | undefined {
  if (!value || typeof value !== "object") return undefined;
  const v = value as Record<string, unknown>;
  const oneOf = (value: unknown, options: string[]) => typeof value === "string" && options.includes(value);
  const nullableString = (value: unknown) => value === null || typeof value === "string";
  const count = (value: unknown) => Number.isSafeInteger(value) && (value as number) >= 0;
  if (v.version !== 1 || !oneOf(v.project, ["untracked", "untrusted", "trusted"]) ||
      !oneOf(v.daemon, ["online", "offline", "wrong_data_dir"]) ||
      !oneOf(v.recording, ["unknown", "awaiting_prompt", "captured"]) ||
      !Array.isArray(v.integrations) || !v.integrations.every((x) => typeof x === "string") ||
      !(v.selected_integration === null || (Array.isArray(v.selected_integration) && v.selected_integration.length === 2 && v.selected_integration.every((x) => typeof x === "string"))) ||
      !nullableString(v.last_activity) || !nullableString(v.last_host) || v.queue_scope !== "device" ||
      !count(v.queued) || !count(v.rejected) || !count(v.write_failures) || typeof v.next_action !== "string") return undefined;
  return v as Readiness;
}

export type Result = { ok: boolean; stdout: string; stderr: string; missing: boolean };

export class Cli {
  constructor(private readonly bin: () => string) {}

  /** Run the CLI. stdin is always closed (after `input`), since some commands read it. */
  run(args: string[], cwd: string, input = "", timeoutMs = 15_000): Promise<Result> {
    return new Promise((resolve) => {
      const child = execFile(this.bin(), args, { cwd, timeout: timeoutMs, encoding: "utf8" }, (err, stdout, stderr) => {
        const missing = (err as NodeJS.ErrnoException | null)?.code === "ENOENT";
        resolve({ ok: !err, stdout: stdout ?? "", stderr: stderr ?? "", missing });
      });
      child.stdin?.on("error", () => {}); // EPIPE when the binary is missing
      child.stdin?.end(input);
    });
  }

  async readiness(cwd: string): Promise<Readiness | undefined> {
    const r = await this.run(["readiness", "--json"], cwd);
    if (!r.ok) return undefined;
    try {
      return parseReadiness(JSON.parse(r.stdout));
    } catch { return undefined; }
  }

  /** Starts the daemon if needed and returns the read-only display connection. */
  async connection(cwd: string): Promise<Connection | undefined> {
    const r = await this.run(["display", "--json"], cwd);
    if (!r.ok) return undefined;
    try {
      return JSON.parse(r.stdout) as Connection;
    } catch {
      return undefined;
    }
  }
}

export const PROJECT_FILE = join(".speedrun", "project.json");

/** The project id for a folder, or undefined if it is not a speedrun project. */
export async function projectId(folder: string): Promise<string | undefined> {
  try {
    return JSON.parse(await readFile(join(folder, PROJECT_FILE), "utf8")).project_id;
  } catch {
    return undefined;
  }
}
