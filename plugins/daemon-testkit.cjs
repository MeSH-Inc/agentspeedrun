// Shared by Bun adapter tests and the Node-driven VS Code host test.
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function readPublishedAddress(home) {
  try {
    return readFileSync(join(home, "addr"), "utf8").trim();
  } catch (error) {
    if (error.code === "ENOENT") return undefined;
    throw error;
  }
}

// Check the child's PID so a different process can never satisfy readiness.
async function waitForDaemon(daemon, address, timeoutMs = 20_000) {
  let spawnError;
  daemon.on("error", (error) => { spawnError = error; });
  const deadline = performance.now() + timeoutMs;
  while (performance.now() < deadline) {
    if (spawnError) throw new Error(`speedrund could not start: ${spawnError.message}`);
    if (daemon.exitCode !== null || daemon.signalCode !== null) {
      throw new Error(`speedrund exited before becoming healthy: ${daemon.exitCode ?? daemon.signalCode}`);
    }
    const bound = typeof address === "number" ? `127.0.0.1:${address}` : address();
    if (bound) {
      const match = /^127\.0\.0\.1:([0-9]{1,5})$/.exec(bound);
      if (!match || Number(match[1]) < 1 || Number(match[1]) > 65535) {
        throw new Error("speedrund published an invalid loopback address");
      }
      try {
        const response = await fetch(`http://${bound}/v1/health`, { signal: AbortSignal.timeout(500) });
        if (response.ok && (await response.json()).pid === daemon.pid) return bound;
      } catch { /* The child may still be starting. */ }
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`speedrund did not become healthy within ${timeoutMs}ms`);
}

module.exports = { readPublishedAddress, waitForDaemon };
