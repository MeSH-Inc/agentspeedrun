// Test harness for host adapters: a throwaway speedrund, data dir and git
// project, driven through the real `speedrun` binary from target/debug.

import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { readPublishedAddress, waitForDaemon } from "./daemon-testkit.cjs";

export { waitForDaemon };

const REPO = resolve(import.meta.dir, "..");
const BIN_DIR = join(REPO, "target", "debug");
export const SPEEDRUN = join(BIN_DIR, "speedrun");

export type Sandbox = {
  project: string;
  /** Run the CLI in the project and return stdout. */
  cli: (...args: string[]) => string;
  /** Commit to the project now. Commit evidence must be made after the run
   * started, so a test that claims a commit makes one once a prompt has
   * started the run. */
  commit: (message: string) => void;
  /** The project's HEAD commit. */
  head: () => string;
  status: () => any;
  events: () => any[];
  close: () => void;
};

/** The GitHub repository every sandbox project names as its `origin`. */
export const REPOSITORY = "acme/recipes";

export type SandboxOptions = {
  /** The category the project is initialized with (default `any-percent`). */
  category?: string;
  /** Extra environment for the daemon, e.g. `SPEEDRUN_GITHUB_API`. */
  daemonEnv?: Record<string, string>;
};

export async function sandbox(options: SandboxOptions = {}): Promise<Sandbox> {
  const root = mkdtempSync(join(tmpdir(), "speedrun-adapter-"));
  const home = join(root, "home");
  const previous = Object.fromEntries(
    ["SPEEDRUN_HOME", "SPEEDRUN_ADDR", "SPEEDRUN_BIN", "SPEEDRUN_NO_AUTOSTART"]
      .map((key) => [key, process.env[key]]),
  );
  const restoreEnv = () => {
    if (process.env.SPEEDRUN_HOME !== home) return;
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  };
  const daemon: ChildProcess = spawn(join(BIN_DIR, "speedrund"), [], {
    stdio: ["ignore", "ignore", "pipe"],
    // A fixed GitHub token, so the verifier never asks `gh` for the user's own.
    env: { ...process.env, ...options.daemonEnv, SPEEDRUN_GITHUB_TOKEN: "sandbox-token", SPEEDRUN_HOME: home, SPEEDRUN_ADDR: "127.0.0.1:0" },
  });
  let stderr = "";
  daemon.stderr?.on("data", (chunk) => { stderr = (stderr + chunk.toString()).slice(-8_192); });
  try {
    const address = await waitForDaemon(daemon, () => readPublishedAddress(home));
    Object.assign(process.env, {
      SPEEDRUN_HOME: home,
      SPEEDRUN_ADDR: address,
      SPEEDRUN_BIN: SPEEDRUN,
      SPEEDRUN_NO_AUTOSTART: "1",
    });
    const env = { ...process.env };
    const project = join(root, "project");
    const git = (...a: string[]) => execFileSync("git", ["-C", project, ...a], { stdio: "pipe" });
    const commit = (message: string) =>
      git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", message);
    execFileSync("git", ["init", "-q", project]);
    git("remote", "add", "origin", `https://github.com/${REPOSITORY}.git`);
    commit("init");
    const cli = (...args: string[]) => execFileSync(SPEEDRUN, args, { cwd: project, encoding: "utf8", env });
    cli("init", "--category", options.category ?? "any-percent");
    return {
      project,
      cli,
      commit,
      head: () => git("rev-parse", "HEAD").toString().trim(),
      status: () => JSON.parse(cli("status", "--json")),
      events: () =>
        cli("log")
          .trim()
          .split("\n")
          .map((l) => JSON.parse(l)),
      close: () => {
        daemon.kill();
        rmSync(root, { recursive: true, force: true });
        restoreEnv();
      },
    };
  } catch (error) {
    daemon.kill();
    rmSync(root, { recursive: true, force: true });
    restoreEnv();
    if (stderr) throw new Error(`${String(error)}\nDaemon stderr:\n${stderr}`, { cause: error });
    throw error;
  }
}

/** A stand-in for GitHub's API that reports every Actions run of
 * [`REPOSITORY`] as passed on `headSha()`, created now. */
export function passingCi(headSha: () => string): { url: string; close: () => void } {
  const server = Bun.serve({
    port: 0,
    fetch(req) {
      const match = new URL(req.url).pathname.match(/^\/repos\/([^/]+\/[^/]+)\/actions\/runs\/(\d+)$/);
      if (!match || match[1] !== REPOSITORY) return Response.json({ message: "Not Found" }, { status: 404 });
      return Response.json({
        id: Number(match[2]),
        status: "completed",
        conclusion: "success",
        head_sha: headSha(),
        created_at: new Date().toISOString(),
      });
    },
  });
  return { url: `http://127.0.0.1:${server.port}`, close: () => server.stop(true) };
}

export const ofType = (events: any[], type: string) => events.filter((e) => e.payload.type === type);
