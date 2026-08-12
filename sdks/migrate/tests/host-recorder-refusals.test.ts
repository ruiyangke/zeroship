import { strict as assert } from "node:assert";
import { test } from "node:test";

import { buildEnvelope } from "../src/host-recorder.js";

// WHY THESE EXIST. `@zeroship/migrate/host-recorder` is the published stand-in
// for `zero-migrate/internal/recorder`, which is NOT published and therefore
// makes @zeroship/vite-plugin unpublishable (#344). Swapping the plugin onto
// this module is only safe if the two REFUSE the same migrations, and measured
// 2026-08-12 they did not: the symbol surface matched 4 of 4 while the
// behaviour diverged on two arms.
//
// The async arm is the one with teeth, and its cost is measured rather than
// argued. Two tables with one `await` between them:
//     zero-migrate    REFUSED ASYNC_UP_UNSUPPORTED
//     this module      ACCEPTED ops=1 tables=["a"]   <- "b" silently dropped
// buildEnvelope RETURNED the truncated envelope; the lost op then threw
// OP_OUTSIDE_RECORDER asynchronously, after the return. So without this refusal
// a creator's migration loses everything after the first await, and the
// envelope looks well-formed.
//
// WHAT THESE TESTS DO NOT COVER: they pin the REFUSALS, not equivalence of the
// happy path. That is checked separately - a plain `up()` produces a
// byte-identical envelope on both modules, and both envelopes drive the addon's
// genArtifacts to byte-identical output (5429b, ok: true).

test("refuses a migration that authors its own down()", () => {
  // Both spellings zero-migrate checks: a named export and a default-object
  // property. Testing only one would leave the other free to regress.
  for (const mod of [
    { up: () => {}, down: () => {} },
    { default: { up: () => {}, down: () => {} } },
  ]) {
    assert.throws(
      () => buildEnvelope(mod as never, { irVersion: 1 }),
      (err: Error & { code?: string }) => {
        assert.equal(err.code, "AUTHORED_DOWN_UNSUPPORTED");
        return true;
      },
    );
  }
});

test("refuses an async up() rather than truncating the envelope", () => {
  assert.throws(
    () => buildEnvelope({ up: async () => {} } as never, { irVersion: 1 }),
    (err: Error & { code?: string }) => {
      assert.equal(err.code, "ASYNC_UP_UNSUPPORTED");
      return true;
    },
  );
});

// The control, and it is the half that makes the two above meaningful: a
// migration that is neither async nor authors a down() must still build. A
// refusal that fired on everything would satisfy both tests above and break the
// product.
test("CONTROL: a plain synchronous up() still builds", () => {
  const env = buildEnvelope({ up: () => {} } as never, {
    irVersion: 1,
    nameFallback: "m",
  });
  assert.equal(env.ir_version, 1);
  assert.equal(env.name, "m");
  assert.deepEqual(env.ops, []);
});
