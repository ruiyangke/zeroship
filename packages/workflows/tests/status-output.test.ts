import assert from "node:assert/strict";
import { test } from "node:test";

import type {
  StatusOutput,
  StatusOutputRef,
  StepOutputRef,
} from "../src/index.ts";

// `pnpm test` runs this file twice: `typecheck:types` compiles it with `tsc`,
// then `tsx` executes it. Both halves are load-bearing. `tsx` strips types
// without checking them, so the `@ts-expect-error` directives below are
// enforced only by the `tsc` pass; the assertions in the tests are enforced
// only by the `tsx` pass.
//
// The descriptor `AppWorkflows::status` builds for a blob-backed final output,
// in `crates/zeroship-workflow/src/service/app.rs`. It reaches a creator through
// `dispatch_json`, so what arrives is this JSON and nothing else.
// `crates/zeroship-workflow/src/service/tests/payloads.rs` asserts the same
// shape from the other side. The two literals are consistent by transcription;
// no generator or shared contract file binds them.
const emitted = {
  kind: "ref",
  ref: "wfblob:sha256:2b1f",
  hash: "2b1f",
  size: 20,
  contentType: "application/json",
} as const;

// The assignment is the assertion: `StatusOutput` has to admit the value the
// status path actually produces. A union built from the in-body `StepOutputRef`
// rejects this, because that shape declares both a different `kind` and readers
// no JSON reply can carry.
const reported: StatusOutput<{ value: string }> = emitted;
const inline: StatusOutput<{ value: string }> = { value: "retained" };

// @ts-expect-error a status descriptor is inert data and declares no readers
type _StatusRefHasNoJson = StatusOutputRef["json"];
// @ts-expect-error the status path emits `kind: "ref"`, not the in-body spelling
const _wrongKind: StatusOutputRef = { ...emitted, kind: "workflow-step-output-ref" };

function locate(output: StatusOutput<{ value: string }>): string | null {
  return (output as StatusOutputRef).kind === "ref"
    ? (output as StatusOutputRef).ref
    : null;
}

test("a creator narrowing a status output on its kind reaches the blob", () => {
  assert.equal(locate(reported), "wfblob:sha256:2b1f");
  // The control: an inline output takes the other arm, so the guard is not
  // answering true for everything.
  assert.equal(locate(inline), null);
});

test("the status descriptor and the in-body ref are spelled apart", () => {
  const inBodyKind: StepOutputRef["kind"] = "workflow-step-output-ref";
  assert.notEqual(inBodyKind, emitted.kind);
  assert.equal(emitted.kind, "ref");
});
