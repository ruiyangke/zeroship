import assert from "node:assert/strict";
import { setTimeout as sleep } from "node:timers/promises";

interface Window { started: number; finished: number }
type Row = Record<string, unknown>;

function milliseconds(value: unknown, field: string): number {
  assert(Number.isSafeInteger(value), `${field} must be an integer Unix millisecond timestamp: ${value}`);
  return value as number;
}

function inWindow(value: number, field: string, window: Window) {
  assert(value >= window.started && value <= window.finished,
    `${field} must fall within its request: ${value} outside ${JSON.stringify(window)}`);
}

export function assertInsertedTimestamps(row: Row, window: Window) {
  const created = milliseconds(row.created_at, "created_at");
  const updated = milliseconds(row.updated_at, "updated_at");
  inWindow(created, "created_at", window);
  inWindow(updated, "updated_at", window);
  assert(updated >= created, "updated_at must not precede created_at");
}

export function assertUpdatedTimestamps(before: Row, after: Row, window: Window) {
  const created = milliseconds(after.created_at, "created_at");
  const updated = milliseconds(after.updated_at, "updated_at");
  assert.equal(created, before.created_at, "updating a row must preserve created_at");
  inWindow(updated, "updated_at", window);
  assert(updated > milliseconds(before.updated_at, "previous updated_at"),
    "an update after the observed clock boundary must advance updated_at");
}

export async function waitForClockAfter(value: unknown): Promise<number> {
  const previous = milliseconds(value, "previous updated_at");
  const signal = AbortSignal.timeout(5_000);
  for (;;) {
    const now = Date.now();
    if (now > previous) return now;
    await sleep(1, undefined, { signal });
  }
}
