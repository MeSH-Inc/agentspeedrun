// Calls into the `speedrun` CLI. All behavior lives there; this extension
// only presents it. No VS Code imports, so it is testable with Bun.

import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { Connection } from "./live";

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
