import assert from "node:assert/strict";
import { test } from "node:test";

import type { StatusOutputRef, StepOutputRef, WorkflowRun } from "../src/index.ts";

// `pnpm test` runs this file twice: `typecheck:types` compiles it with `tsc`,
// then `tsx` executes it. Both halves are load-bearing. `tsx` strips types
// without checking them, so the `@ts-expect-error` directives below are
// enforced only by the `tsc` pass; the assertions in the tests are enforced
// only by the `tsx` pass.
//
// The descriptor `AppWorkflows::status` builds for a run's final output, in
// `crates/zeroship-workflow/src/service/app.rs`. It reaches a creator through
// `dispatch_json`, so what arrives is this JSON and nothing else.
// `crates/zeroship-workflow-runner/src/outputs/tests.rs` asserts the same shape
// from the other side. The two literals are consistent by transcription; no
// generator or shared contract file binds them.
const emitted = {
  kind: "ref",
  ref: "wfblob:sha256:2b1f",
  hash: "2b1f",
  size: 20,
  contentType: "application/json",
} as const;

// The assignment is the assertion: the declared shape has to admit the value the
// status path actually produces. The in-body `StepOutputRef` rejects this,
// because that shape declares both a different `kind` and readers no JSON reply
// can carry.
const reported: StatusOutputRef = emitted;

// @ts-expect-error a status descriptor is inert data and declares no readers
type _StatusRefHasNoJson = StatusOutputRef["json"];
// @ts-expect-error the status path emits `kind: "ref"`, not the in-body spelling
const _wrongKind: StatusOutputRef = { ...emitted, kind: "workflow-step-output-ref" };

type ReportedOutput = Awaited<ReturnType<WorkflowRun<{ value: string }>["status"]>>["output"];

// A run's result is a blob whatever it weighs, so `status` reports a descriptor
// or nothing. The run's own `Output` parameter never reaches this field: a
// creator holding the run's declared result type still gets the descriptor, and
// reads the value through `readOutput`.
const absent: ReportedOutput = undefined;
const located: ReportedOutput = emitted;
// @ts-expect-error the declared result type is not what `status` reports
const _inline: ReportedOutput = { value: "retained" };

function locate(output: ReportedOutput): string | null {
  return output ? output.ref : null;
}

test("a creator reading a status output reaches the blob without narrowing a union", () => {
  assert.equal(locate(located), "wfblob:sha256:2b1f");
  // The control: a run that returned nothing reports no output, so the reader
  // is answering from the descriptor rather than for everything.
  assert.equal(locate(absent), null);
});

test("the status descriptor and the in-body ref are spelled apart", () => {
  const inBodyKind: StepOutputRef["kind"] = "workflow-step-output-ref";
  assert.notEqual(inBodyKind, emitted.kind);
  assert.equal(emitted.kind, "ref");
  assert.equal(reported.kind, "ref");
});
