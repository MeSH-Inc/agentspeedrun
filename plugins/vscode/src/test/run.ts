// Runs the extension inside a real VS Code (downloaded by @vscode/test-electron)
// against a real speedrund and a throwaway project. Needs `cargo build` first,
// and a display (CI uses xvfb-run).

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { runTests } from "@vscode/test-electron";
import { readPublishedAddress, waitForDaemon } from "../../../daemon-testkit.cjs";

async function main() {
  const repo = resolve(__dirname, "../../../..");
  const bin = join(repo, "target", "debug");
  const root = mkdtempSync(join(tmpdir(), "speedrun-vscode-"));
  const env = {
    SPEEDRUN_HOME: join(root, "home"),
    SPEEDRUN_ADDR: "127.0.0.1:0",
    SPEEDRUN_BIN: join(bin, "speedrun"),
    SPEEDRUN_NO_AUTOSTART: "1",
  };
  Object.assign(process.env, env);
  const daemon = spawn(join(bin, "speedrund"), [], { stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  daemon.stderr?.on("data", (chunk) => { stderr = (stderr + chunk.toString()).slice(-8_192); });
  const project = join(root, "project");
  try {
    try {
      env.SPEEDRUN_ADDR = await waitForDaemon(daemon, () => readPublishedAddress(env.SPEEDRUN_HOME));
      process.env.SPEEDRUN_ADDR = env.SPEEDRUN_ADDR;
    } catch (error) {
      if (stderr) throw new Error(`${String(error)}\nDaemon stderr:\n${stderr}`, { cause: error });
      throw error;
    }
    execFileSync("git", ["init", "-q", project]);
    execFileSync("git", ["-C", project, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
    execFileSync(env.SPEEDRUN_BIN, ["init", "--category", "any-percent"], { cwd: project });
    await runTests({
      version: process.env.VSCODE_VERSION ?? "stable",
      extensionDevelopmentPath: resolve(__dirname, "../.."),
      extensionTestsPath: resolve(__dirname, "suite/index"),
      launchArgs: [project, "--disable-extensions", "--disable-workspace-trust", "--no-sandbox", "--disable-gpu"],
      extensionTestsEnv: { ...env, SPEEDRUN_TEST_PROJECT: project },
    });
  } finally {
    daemon.kill();
    rmSync(root, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
