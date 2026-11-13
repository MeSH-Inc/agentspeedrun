import type { ChildProcess } from "node:child_process";

export function readPublishedAddress(home: string): string | undefined;
export function waitForDaemon(
  daemon: ChildProcess,
  address: number | (() => string | undefined),
  timeoutMs?: number,
): Promise<string>;
