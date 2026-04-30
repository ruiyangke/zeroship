// packages/zeroship-rpc-client/src/idempotency.ts
//
// UUIDv7 generator. RFC 9562 §5.7:
//
//   - 48-bit unix-ms timestamp (big-endian)
//   - 4-bit version (0b0111)
//   - 12-bit random
//   - 2-bit variant (0b10)
//   - 62-bit random
//
// We only use this for `Idempotency-Key` and `X-Request-Id` headers.
// Both are advisory — an off-by-one nanosecond or non-monotonic
// timestamp doesn't break correctness; the kernel hashes the key
// regardless. So we don't need to track per-thread monotonicity.

/** Generate a fresh UUIDv7 string. */
export function newUuidV7(): string {
  // 16 random bytes; we'll overwrite the first 6 with the timestamp.
  const bytes = randomBytes(16);

  const ms = Date.now();
  // High 48 bits = ms timestamp, big-endian.
  // Math.floor(ms / 2^32) for top 16 bits; (ms & 0xffffffff) for low 32.
  const high = Math.floor(ms / 0x100000000);
  const low = ms >>> 0;
  bytes[0] = (high >>> 8) & 0xff;
  bytes[1] = high & 0xff;
  bytes[2] = (low >>> 24) & 0xff;
  bytes[3] = (low >>> 16) & 0xff;
  bytes[4] = (low >>> 8) & 0xff;
  bytes[5] = low & 0xff;

  // Set version (0b0111) on byte 6's high nibble.
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  // Set variant (0b10) on byte 8's high two bits.
  bytes[8] = (bytes[8] & 0x3f) | 0x80;

  // Format `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0"));
  return (
    hex.slice(0, 4).join("") +
    "-" +
    hex.slice(4, 6).join("") +
    "-" +
    hex.slice(6, 8).join("") +
    "-" +
    hex.slice(8, 10).join("") +
    "-" +
    hex.slice(10, 16).join("")
  );
}

/** Cross-environment 16-byte random buffer. */
function randomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  if (
    typeof globalThis !== "undefined" &&
    typeof globalThis.crypto?.getRandomValues === "function"
  ) {
    globalThis.crypto.getRandomValues(out);
    return out;
  }
  // Fallback for older Node versions without globalThis.crypto.
  for (let i = 0; i < n; i++) out[i] = (Math.random() * 256) & 0xff;
  return out;
}
