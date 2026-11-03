// Live-display client: follows speedrund's /v1/live/stream and ticks clocks
// locally (docs/LIVE_DISPLAY.md). No VS Code imports, so it is testable with Bun.

import type { LiveSnapshot, LiveRun, LiveMilestone, Timing2 as Timing } from "./protocol.generated";
import { validateSnapshot } from "./validate.generated";
export type { LiveSnapshot, LiveRun, LiveMilestone } from "./protocol.generated";
export type { Timing2 as Timing } from "./protocol.generated";

/** The clocks at `now`, extrapolated from a snapshot taken at `asOf`. Mirrors `LiveRun::timing_at`. */
export function timingAt(run: LiveRun, asOf: Date, now: Date): Timing {
  const dt = Math.max(0, now.getTime() - asOf.getTime());
  const r = run.running;
  const stop = r.human_stops_at ? Date.parse(r.human_stops_at) : Infinity;
  const humanDt = Math.max(0, Math.min(now.getTime(), stop) - asOf.getTime());
  return {
    wall_ms: run.timing.wall_ms + (r.wall ? dt : 0),
    timed_ms: run.timing.timed_ms + (r.timed ? dt : 0),
    agent_ms: run.timing.agent_ms + (r.agent ? dt : 0),
    human_ms: run.timing.human_ms + (r.human ? humanDt : 0),
  };
}

export function fmtMs(ms: number): string {
  const s = Math.floor(Math.max(0, ms) / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${Math.floor(s / 3600)}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}`;
}

export function fmtDelta(ms: number): string {
  const s = Math.floor(Math.abs(ms) / 1000);
  return `${ms < 0 ? "-" : "+"}${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** Status bar text, e.g. `$(watch) Any% 0:12:03 · 1/2 · -0:42`. */
export function statusText(run: LiveRun, asOf: Date, now: Date): string {
  const t = timingAt(run, asOf, now);
  const parts = [`${run.category.name} ${fmtMs(t.timed_ms)}`, `${run.progress.counted}/${run.progress.total}`];
  const last = [...run.milestones].reverse().find((m) => m.delta_ms !== null);
  if (last?.delta_ms != null) parts.push(fmtDelta(last.delta_ms));
  const icon =
    run.status === "completed" ? "$(pass-filled)" : run.status === "paused" ? "$(debug-pause)" : "$(watch)";
  return `${icon} ${parts.join(" · ")}`;
}

/** Incremental server-sent-events parser. Feed it chunks; it returns complete events. */
export class SseParser {
  private buffer = "";
  private event = "";
  private data: string[] = [];

  push(chunk: string): { event: string; data: string }[] {
    this.buffer += chunk;
    const out: { event: string; data: string }[] = [];
    let nl: number;
    while ((nl = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, nl).replace(/\r$/, "");
      this.buffer = this.buffer.slice(nl + 1);
      if (line === "") {
        if (this.data.length) out.push({ event: this.event || "message", data: this.data.join("\n") });
        this.event = "";
        this.data = [];
      } else if (line.startsWith("event:")) {
        this.event = line.slice(6).trim();
      } else if (line.startsWith("data:")) {
        this.data.push(line.slice(5).replace(/^ /, ""));
      }
    }
    return out;
  }
}

export type LiveState = "connected" | "stale" | "disconnected" | "incompatible" | "corrupt-data";

export type Connection = { base_url: string; token: string };

/**
 * Follows the live stream, reconnecting with backoff. `connect` is called
 * before every attempt so the caller can (re)start the daemon and refresh
 * the token. Snapshots with a lower cursor than the last one are dropped.
 */
export class LiveClient {
  private abort?: AbortController;
  private stopped = false;
  private lastCursor = -1;
  private streamKey?: string;
  private lastReceived = 0;
  private staleTimer?: ReturnType<typeof setInterval>;
  latest?: LiveSnapshot;

  constructor(
    private readonly connect: () => Promise<Connection | undefined>,
    private readonly onSnapshot: (s: LiveSnapshot) => void,
    private readonly onState: (state: LiveState) => void = () => {},
  ) {}

  start(): void {
    this.staleTimer = setInterval(() => {
      if (this.lastReceived && Date.now() - this.lastReceived > 90_000) this.onState("stale");
    }, 5000);
    void this.loop();
  }

  stop(): void {
    this.stopped = true;
    this.abort?.abort();
    if (this.staleTimer) clearInterval(this.staleTimer);
  }

  private async loop() {
    let backoff = 500;
    while (!this.stopped) {
      try {
        const conn = await this.connect();
        if (conn) {
          await this.follow(conn);
          backoff = 500;
        }
      } catch {
        // Fall through to reconnect.
      }
      if (this.stopped) break;
      this.onState("disconnected");
      await new Promise((r) => setTimeout(r, backoff));
      backoff = Math.min(backoff * 2, 10_000);
    }
  }

  private async follow(conn: Connection) {
    this.abort = new AbortController();
    const res = await fetch(`${conn.base_url}/v1/live/stream`, {
      headers: { Authorization: `Bearer ${conn.token}` },
      signal: this.abort.signal,
    });
    if (!res.ok || !res.body) throw new Error(`stream refused: ${res.status}`);
    const parser = new SseParser();
    const decoder = new TextDecoder();
    for await (const chunk of res.body as unknown as AsyncIterable<Uint8Array>) {
      for (const ev of parser.push(decoder.decode(chunk, { stream: true }))) {
        if (ev.event !== "snapshot") continue;
        let value: unknown;
        try { value = JSON.parse(ev.data); } catch { this.onState("corrupt-data"); continue; }
        if (value && typeof value === "object" && "protocol" in value && value.protocol !== 2) {
          this.onState("incompatible"); continue;
        }
        if (!validateSnapshot(value) || !Number.isFinite(Date.parse((value as LiveSnapshot).as_of))) {
          this.onState("corrupt-data"); continue;
        }
        const snap = value as LiveSnapshot;
        const key = `${snap.source.kind}:${snap.source.id}:${snap.source.generation}`;
        if (key !== this.streamKey) { this.streamKey = key; this.lastCursor = -1; }
        if (snap.cursor < this.lastCursor) continue;
        this.lastCursor = snap.cursor;
        this.latest = snap;
        this.lastReceived = Date.now();
        this.onSnapshot(snap);
        this.onState(snap.runs.some((r) => (r.errors?.length ?? 0) > 0) ? "corrupt-data" : "connected");
      }
    }
  }
}
