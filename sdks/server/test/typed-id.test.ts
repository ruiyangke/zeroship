import { describe, test } from "node:test";
import assert from "node:assert/strict";

import {
  base62ToUuid,
  isTypedId,
  parseTypedId,
  retagTypedId,
  typedIdFromStableSeed,
  typedIdFromUuid,
  uuidFromStableSeed,
  uuidToBase62,
} from "../src/typed-id.js";

describe("typed-id helpers", () => {
  test("matches zeroship_core base62 encoding for a known UUID", () => {
    const uuid = "00000000-0000-7000-8000-000000000001";
    const encoded = uuidToBase62(uuid);

    assert.equal(encoded, "000000002dwHTRTFRxWLTN");
    assert.equal(base62ToUuid(encoded), uuid);
    assert.equal(typedIdFromUuid("usr", uuid), "usr_000000002dwHTRTFRxWLTN");
  });

  test("parses and retags by preserving the embedded UUID", () => {
    const appId = typedIdFromUuid("app", "018f6b34-09e5-7c44-9f3e-03290837b618");
    const prjId = retagTypedId(appId, "prj");

    assert.equal(prjId, "prj_02wXA617NLbuNBLj2Kzysy");
    assert.deepEqual(parseTypedId(prjId, "prj"), {
      prefix: "prj",
      encoded: "02wXA617NLbuNBLj2Kzysy",
      uuid: "018f6b34-09e5-7c44-9f3e-03290837b618",
    });
  });

  test("stable seed derivation is deterministic and typed", () => {
    const seed = "zeroship-builder:test-thread";

    assert.equal(uuidFromStableSeed(seed), uuidFromStableSeed(seed));
    assert.equal(
      typedIdFromStableSeed("prj", seed),
      typedIdFromStableSeed("prj", seed),
    );
    assert.equal(isTypedId(typedIdFromStableSeed("prj", seed), "prj"), true);
    assert.equal(isTypedId(typedIdFromStableSeed("prj", seed), "usr"), false);
  });
});
