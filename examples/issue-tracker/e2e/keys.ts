/**
 * Collision-proof product keys for fixtures.
 *
 * Every spec used to build one as `PRE${String(Date.now()).slice(-5)}`. The
 * last five digits of epoch-ms wrap every 100_000 ms -- about a minute and a
 * half -- so two runs offset by that much produced the SAME key, and
 * `products.create` answered 409 on the unique index. The suite therefore got
 * less reliable the more often it ran, which is the opposite of what a suite
 * is for, and the failure surfaced as an unrelated-looking assertion on the
 * first RPC of whichever spec happened to lose.
 *
 * The server's rule is `/^[A-Z][A-Z0-9]{1,9}$/` -- a letter, then one to nine
 * more uppercase alphanumerics, ten characters in total. That is the whole
 * budget, and it is spent on RANDOMNESS rather than on a timestamp:
 *
 *   letter    1 char   caller's mnemonic, so a failing key names its spec
 *   random    9 chars  base36, 36^9 ~= 1.0e14 possibilities
 *
 * A timestamp was the obvious choice and was wrong. Millisecond-granularity
 * leaves nothing to distinguish two fixtures created in the same tick, so it
 * needs a counter beside it, and a 1-char counter only has 36 values: an
 * earlier draft of this file generated 72 unique keys out of 500 calls. The
 * key does not need to encode WHEN it was made, only to be unlike every other
 * key, so the whole budget goes to entropy.
 *
 * Math.random is sufficient here -- nothing about a test fixture name is a
 * security boundary, and uniqueness is the only requirement.
 */

const KEY_PATTERN = /^[A-Z][A-Z0-9]{1,9}$/;

export function productKey(mnemonic: string): string {
  const letter = mnemonic.toUpperCase().replace(/[^A-Z]/g, "").slice(0, 1) || "X";
  let random = "";
  while (random.length < 9) {
    random += Math.random().toString(36).slice(2).toUpperCase();
  }
  const key = `${letter}${random.slice(0, 9)}`;

  // Fail here rather than at the RPC. A malformed key comes back as a generic
  // 400 from `requireProductKey` several frames away, where it reads as a
  // broken procedure rather than a broken fixture.
  if (!KEY_PATTERN.test(key)) {
    throw new Error(`generated product key ${key} does not match ${KEY_PATTERN}`);
  }
  return key;
}
