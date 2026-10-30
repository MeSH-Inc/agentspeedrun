// Runs the extension inside a real VS Code (downloaded by @vscode/test-electron)
// against a real speedrund and a throwaway project. Needs `cargo build` first,
// and a display (CI uses xvfb-run).

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { runTests } from "@vscode/test-electron";

async function main() {
  const repo = resolve(__dirname, "../../../..");
  const bin = join(repo, "target", "debug");
  const root = mkdtempSync(join(tmpdir(), "speedrun-vscode-"));
  const port = 20000 + Math.floor(Math.random() * 20000);
  const env = {
    SPEEDRUN_HOME: join(root, "home"),
    SPEEDRUN_ADDR: `127.0.0.1:${port}`,
    SPEEDRUN_BIN: join(bin, "speedrun"),
    SPEEDRUN_NO_AUTOSTART: "1",
  };
  Object.assign(process.env, env);
  const daemon = spawn(join(bin, "speedrund"), [], { stdio: "ignore" });
  const project = join(root, "project");
  try {
    for (let i = 0; ; i++) {
      try {
        if ((await fetch(`http://127.0.0.1:${port}/v1/health`)).ok) break;
      } catch {}
      if (i > 50) throw new Error("speedrund did not start; run `cargo build` first");
      await new Promise((r) => setTimeout(r, 50));
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
