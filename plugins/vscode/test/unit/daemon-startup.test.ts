import { expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { waitForDaemon } from "../../../testkit";

test("daemon startup waits for the right process on a slow host", async () => {
  const child = spawn(process.execPath, ["-e", "setTimeout(() => {}, 10000)"], { stdio: "ignore" });
  const readyAt = performance.now() + 2_800;
  let requests = 0;
  const server = Bun.serve({
    port: 0,
    fetch() {
      requests += 1;
      return Response.json({ pid: performance.now() >= readyAt ? child.pid : -1 });
    },
  });
  try {
    await waitForDaemon(child, server.port!, 10_000);
    expect(requests).toBeGreaterThan(1);
  } finally {
    child.kill();
    server.stop(true);
  }
});

test("daemon startup reports a child that exits before becoming healthy", async () => {
  const child = spawn(process.execPath, ["-e", "process.exit(23)"], { stdio: "ignore" });
  const server = Bun.serve({ port: 0, fetch: () => new Response("", { status: 503 }) });
  try {
    await expect(waitForDaemon(child, server.port!, 5_000)).rejects.toThrow("exited before becoming healthy: 23");
  } finally {
    child.kill();
    server.stop(true);
  }
});
