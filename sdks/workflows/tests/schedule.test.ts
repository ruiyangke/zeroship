import assert from "node:assert/strict";
import { describe, test } from "node:test";

import {
  compileSchedule,
  cronExpr,
  every,
  InvalidScheduleError,
  schedule,
  Workflow,
  type NormalizedScheduleDescriptor,
} from "../src/index.ts";

function serializable(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value));
}

describe("compileSchedule", () => {
  test("normalizes fluent daily cron schedules", () => {
    assert.deepEqual(
      serializable(compileSchedule(every.day.at("03:00", "America/New_York"))),
      {
        kind: "cron",
        cron_expr: "0 3 * * *",
        tz: "America/New_York",
        overlap: "allow",
        catchUp: { mode: "skip" },
      },
    );
  });

  test("normalizes raw cron strings and cronExpr timezone forms", () => {
    assert.deepEqual(
      serializable(compileSchedule("*/5 * * * *")),
      {
        kind: "cron",
        cron_expr: "*/5 * * * *",
        tz: "UTC",
        overlap: "allow",
        catchUp: { mode: "skip" },
      },
    );
    assert.deepEqual(
      serializable(compileSchedule(cronExpr("@daily", "Europe/London"))),
      {
        kind: "cron",
        cron_expr: "0 0 * * *",
        tz: "Europe/London",
        overlap: "allow",
        catchUp: { mode: "skip" },
      },
    );
  });

  test("normalizes interval schedules to interval_ms", () => {
    assert.deepEqual(
      serializable(compileSchedule(every(15, "minutes"))),
      {
        kind: "interval",
        interval_ms: 900_000,
        anchor: "epoch",
        overlap: "allow",
        catchUp: { mode: "skip" },
      },
    );
    assert.deepEqual(
      serializable(compileSchedule(every(5).minutes())),
      {
        kind: "interval",
        interval_ms: 300_000,
        anchor: "epoch",
        overlap: "allow",
        catchUp: { mode: "skip" },
      },
    );
  });

  test("preserves cron timezone descriptors for DST edge schedules", () => {
    const vectors: Array<{
      label: string;
      descriptor: NormalizedScheduleDescriptor;
      expected: NormalizedScheduleDescriptor;
    }> = [
      {
        label: "spring-forward nonexistent local time",
        descriptor: compileSchedule(every().day().at("02:30", "America/New_York")),
        expected: {
          kind: "cron",
          cron_expr: "30 2 * * *",
          tz: "America/New_York",
          overlap: "allow",
          catchUp: { mode: "skip" },
        },
      },
      {
        label: "fall-back ambiguous local time",
        descriptor: compileSchedule(every().day().at("01:30", "America/New_York")),
        expected: {
          kind: "cron",
          cron_expr: "30 1 * * *",
          tz: "America/New_York",
          overlap: "allow",
          catchUp: { mode: "skip" },
        },
      },
    ];

    for (const vector of vectors) {
      assert.deepEqual(serializable(vector.descriptor), vector.expected, vector.label);
    }
  });

  test("rejects bad timezone, sub-minute cron, and malformed cron", () => {
    assert.throws(
      () => cronExpr("0 9 * * *", "Mars/Base"),
      InvalidScheduleError,
    );
    assert.throws(
      () => compileSchedule("* * * * * *"),
      InvalidScheduleError,
    );
    assert.throws(
      () => compileSchedule("61 * * * *"),
      InvalidScheduleError,
    );
  });

  test("schedule returns a serializable registration with normalized policies", () => {
    class NightlyReport extends Workflow<{ region: string }, void> {
      run(): void {}
    }

    const registration = schedule({
      name: "nightly-report-us",
      schedule: every.day.at("03:00", "America/New_York"),
      workflow: NightlyReport,
      input: { region: "us" },
      overlap: "skipIfRunning",
      catchUp: { mode: "backfill", max: 3 },
    });

    assert.deepEqual(serializable(registration), {
      name: "nightly-report-us",
      workflowName: "NightlyReport",
      input: { region: "us" },
      overlap: "skipIfRunning",
      catchUp: { mode: "backfill", max: 3 },
      schedule: {
        kind: "cron",
        cron_expr: "0 3 * * *",
        tz: "America/New_York",
        overlap: "skipIfRunning",
        catchUp: { mode: "backfill", max: 3 },
      },
    });
  });
});
