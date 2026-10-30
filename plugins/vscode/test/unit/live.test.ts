import { afterAll, beforeAll, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { sandbox, type Sandbox } from "../../../testkit";
import { Cli } from "../../src/cli";
import { fmtDelta, fmtMs, LiveClient, type LiveSnapshot, SseParser, statusText, timingAt } from "../../src/live";

// The committed example the Rust side generates and tests against.
const example: LiveSnapshot = JSON.parse(
  readFileSync(join(import.meta.dir, "../../../../schema/examples/live-snapshot.json"), "utf8"),
);

test("timingAt advances only the running clocks", () => {
  const run = example.runs[0];
  const asOf = new Date(example.as_of);
  const later = new Date(asOf.getTime() + 60_000);
  const t = timingAt(run, asOf, later);
  expect(t.wall_ms).toBe(run.timing.wall_ms + 60_000);
  expect(t.agent_ms).toBe(run.timing.agent_ms + 60_000);
  expect(t.human_ms).toBe(run.timing.human_ms); // agents are working
  expect(timingAt(run, asOf, new Date(asOf.getTime() - 5000))).toEqual(run.timing); // never runs backwards
});

test("human time stops at the idle cap", () => {
  const run = structuredClone(example.runs[0]);
  const asOf = new Date(example.as_of);
  run.running = { wall: true, timed: true, agent: false, human: true, human_stops_at: new Date(asOf.getTime() + 30_000).toISOString() };
  expect(timingAt(run, asOf, new Date(asOf.getTime() + 600_000)).human_ms).toBe(run.timing.human_ms + 30_000);
});

test("status text shows category, timer, progress and the latest delta", () => {
  const run = example.runs[0];
  const asOf = new Date(example.as_of);
  expect(statusText(run, asOf, asOf)).toBe("$(watch) Any% 0:50:00 · 1/2 · -3:00");
  expect(fmtMs(3_723_000)).toBe("1:02:03");
  expect(fmtDelta(42_000)).toBe("+0:42");
});

test("SSE parser handles split chunks, CRLF and multi-line data", () => {
  const p = new SseParser();
  expect(p.push("event: snap")).toEqual([]);
  expect(p.push("shot\r\nid: 3\r\ndata: {\"a\":\r\ndata: 1}\r\n")).toEqual([]);
  expect(p.push("\r\n: keep-alive\n\nevent: snapshot\ndata: 2\n\n")).toEqual([
    { event: "snapshot", data: '{"a":\n1}' },
    { event: "snapshot", data: "2" },
  ]);
});

let sb: Sandbox;
beforeAll(async () => {
  sb = await sandbox();
});
afterAll(() => sb?.close());

test("missing CLI is reported, not thrown", async () => {
  const r = await new Cli(() => "/nonexistent/speedrun").run(["status"], sb.project);
  expect(r).toMatchObject({ ok: false, missing: true });
});

test("live client follows the real stream and sees pushed updates", async () => {
  const cli = new Cli(() => process.env.SPEEDRUN_BIN!);
  const snaps: LiveSnapshot[] = [];
  const client = new LiveClient(() => cli.connection(sb.project), (s) => snaps.push(s));
  client.start();
  const until = async (cond: () => boolean) => {
    for (let i = 0; i < 100 && !cond(); i++) await Bun.sleep(30);
    expect(cond()).toBe(true);
  };
  try {
    await until(() => snaps.length >= 1);
    expect(snaps[0].runs).toEqual([]);
    await cli.run(["ingest", "prompt", "--host", "test", "--session", "s"], sb.project, "build it");
    await until(() => snaps.at(-1)!.runs.length === 1);
    const last = snaps.at(-1)!;
    expect(last.cursor).toBeGreaterThan(snaps[0].cursor);
    expect(last.runs[0].category.name).toBe("Any%");
    expect(last.runs[0].active_agents).toBe(1);
  } finally {
    client.stop();
  }
});
