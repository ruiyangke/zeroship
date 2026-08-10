// Per-demo summary reporter.
//
// The point of this tier is to answer one question at a glance: "how many of
// our demos actually work?" A flat list of 40 test lines does not answer it, so
// this reporter groups every result line by the demo whose suite produced it
// and prints one row per demo plus a headline count.
//
// It is driven by vitest's own reported results (onTestCaseResult), not by
// anything the suites self-report, so a suite cannot flatter itself.
//
// Two rules encoded here:
//
//   * A demo with ANY failed test is BROKEN. Partial credit would let a demo
//     whose core interaction is dead pass on the strength of "the page loaded".
//   * A demo in the registry that produced NO result line at all is reported as
//     NO RESULT and makes the run incomplete. A demo that quietly disappears
//     from the run is exactly as invisible as a skip, which is the hole this
//     tier exists to fill.

import { basename } from "node:path";
import type { Reporter } from "vitest/reporters";
import { DEMOS } from "./demos.js";

interface DemoTally {
  passed: number;
  failed: number;
  skipped: number;
  firstFailure?: { name: string; message: string };
}

function demoNameFromModuleId(moduleId: string): string {
  return basename(moduleId).replace(/\.test\.ts$/, "");
}

/**
 * The one line worth putting in the summary.
 *
 * Assertion messages here open with the "browser diagnostics:" header we
 * attached, so the literal first line says nothing. Prefer the first line that
 * names an actual error — that is almost always the server-side throw, which is
 * the thing a reader needs — and fall back to the first line with content.
 */
function headlineFrom(text: string | undefined): string {
  if (!text) return "(no message)";
  const lines = text
    .split("\n")
    .map((l) => l.trim())
    // Drop our own section headers ("browser diagnostics:", "console errors:")
    // — they are structure, not the finding.
    .filter((l) => l.length > 0 && !l.endsWith(":"));
  const named =
    lines.find((l) => /^[A-Z]\w*Error: /.test(l)) ??
    lines.find((l) => /^\d{3} http/.test(l)) ??
    lines[0];
  return (named ?? "(no message)").slice(0, 160);
}

export default class DemoSummaryReporter implements Reporter {
  private tallies = new Map<string, DemoTally>();

  private tally(demo: string): DemoTally {
    let t = this.tallies.get(demo);
    if (!t) {
      t = { passed: 0, failed: 0, skipped: 0 };
      this.tallies.set(demo, t);
    }
    return t;
  }

  onTestCaseResult(testCase: {
    module: { moduleId: string };
    fullName: string;
    result: () => { state: string; errors?: ReadonlyArray<{ message?: string }> };
  }): void {
    const demo = demoNameFromModuleId(testCase.module.moduleId);
    const t = this.tally(demo);
    const result = testCase.result();
    if (result.state === "passed") t.passed++;
    else if (result.state === "skipped") t.skipped++;
    else {
      t.failed++;
      if (!t.firstFailure) {
        t.firstFailure = {
          name: testCase.fullName,
          message: headlineFrom(result.errors?.[0]?.message),
        };
      }
    }
  }

  onTestRunEnd(): void {
    const lines: string[] = [];
    const width = Math.max(12, ...DEMOS.map((d) => d.name.length));

    let working = 0;
    let broken = 0;
    let missing = 0;

    lines.push("");
    lines.push("=".repeat(78));
    lines.push("  DEMO STATUS — browser tier (examples/*)");
    lines.push("=".repeat(78));

    for (const demo of DEMOS) {
      const t = this.tallies.get(demo.name);
      const pad = demo.name.padEnd(width);

      if (!t || t.passed + t.failed + t.skipped === 0) {
        missing++;
        lines.push(`  ${pad}  NO RESULT   suite produced no result line`);
        continue;
      }
      // A skipped test here is a defect in the suite, not a demo state: this
      // tier is not allowed to skip. Surface it rather than absorbing it.
      if (t.skipped > 0) {
        lines.push(
          `  ${pad}  !! ${t.skipped} test(s) SKIPPED — this tier must never skip; fix the suite`,
        );
      }
      if (t.failed > 0) {
        broken++;
        lines.push(`  ${pad}  BROKEN      ${t.passed} passed, ${t.failed} failed`);
        if (t.firstFailure) {
          lines.push(`  ${" ".repeat(width)}    ↳ ${t.firstFailure.name}`);
          lines.push(`  ${" ".repeat(width)}      ${t.firstFailure.message}`);
        }
      } else {
        working++;
        lines.push(`  ${pad}  WORKING     ${t.passed} passed`);
      }
    }

    // Anything that reported but is not in the registry (a stray suite file).
    for (const [name, t] of this.tallies) {
      if (DEMOS.some((d) => d.name === name)) continue;
      lines.push(`  ${name.padEnd(width)}  UNREGISTERED  ${t.passed} passed, ${t.failed} failed`);
    }

    lines.push("-".repeat(78));
    lines.push(
      `  ${working} of ${DEMOS.length} demos actually work` +
        (broken > 0 ? ` · ${broken} broken` : "") +
        (missing > 0 ? ` · ${missing} produced no result` : ""),
    );
    lines.push("=".repeat(78));
    lines.push("");

    // eslint-disable-next-line no-console
    console.log(lines.join("\n"));
  }
}
