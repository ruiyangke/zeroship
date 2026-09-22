import assert from "node:assert/strict";
import { test } from "node:test";

import type { WorkflowRun, WorkflowRunState } from "../src/index.ts";

// `pnpm test` runs this file twice: `typecheck:types` compiles it with `tsc`,
// then `tsx` executes it. Both halves are load-bearing. `tsx` strips types
// without checking them, so the `@ts-expect-error` directives below are
// enforced only by the `tsc` pass; the assertions in the test are enforced
// only by the `tsx` pass.
//
// What a creator receives is `RunStatus` from
// `crates/zeroship-workflow/src/operations.rs`, serialised by `serde` and
// handed across by `dispatch_json` in
// `crates/zeroship-workflow-v8/src/v8_class.rs`. `state` is `RunState` from
// `crates/zeroship-core/src/workflow_coordination/lifecycle.rs`, whose
// `rename_all = "camelCase"` spells its continued variant `continuedAsNew`;
// the successor field carries `rename = "continuedAsNew"` beside
// `skip_serializing_if = "Option::is_none"`, so it is absent rather than null
// on a run that started no successor. The spellings below are consistent with
// those attributes by transcription; no generator ties the two sides together.
type Status = Awaited<ReturnType<WorkflowRun<{ value: string }>["status"]>>;

// The assignments are the assertions: the reply type has to admit the state a
// continued run rests in, and the successor key beside it. Excess property
// checking is what makes the second literal fail if the field is not declared.
const continued: Status = { state: "continuedAsNew", continuedAsNew: "wfr_2f" };
const completed: Status = { state: "completed" };

// @ts-expect-error the successor is absent on a run that started none, not null
const _nulledSuccessor: Status = { state: "continuedAsNew", continuedAsNew: null };
// @ts-expect-error the state is the camelCase wire spelling, not the Rust identifier
const _rustSpelling: WorkflowRunState = "ContinuedAsNew";

// The declared return type is the assertion that the successor is a run id
// rather than `unknown`: a creator can use it without a cast.
function successorOf(status: Status): string | null {
  return status.state === "continuedAsNew" ? (status.continuedAsNew ?? null) : null;
}

test("a creator reads the successor off a continued run", () => {
  assert.equal(successorOf(continued), "wfr_2f");
  // The control: a completed run takes the other arm, so the guard is not
  // answering with a successor for everything.
  assert.equal(successorOf(completed), null);
});
