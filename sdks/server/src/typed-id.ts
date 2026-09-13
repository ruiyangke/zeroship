const BASE36 = "0123456789abcdefghijklmnopqrstuvwxyz";
const TYPED_ID_RE = /^([a-z]{3,4})_([0-9a-z]{25})$/;
const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export interface ParsedTypedId {
  prefix: string;
  uuid: string;
  encoded: string;
}

function assertPrefix(prefix: string): void {
  if (!/^[a-z]{3,4}$/.test(prefix)) {
    throw new Error(`invalid typed-id prefix: ${prefix}`);
  }
}

function normalizeUuid(uuid: string): string {
  const lower = uuid.toLowerCase();
  if (!UUID_RE.test(lower)) {
    throw new Error(`invalid UUID: ${uuid}`);
  }
  return lower;
}

function uuidToBigInt(uuid: string): bigint {
  return BigInt(`0x${normalizeUuid(uuid).replaceAll("-", "")}`);
}

function bigIntToUuid(value: bigint): string {
  const hex = value.toString(16).padStart(32, "0");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20),
  ].join("-");
}

export function uuidToBase36(uuid: string): string {
  let n = uuidToBigInt(uuid);
  let out = "";
  for (let i = 0; i < 25; i++) {
    out = BASE36[Number(n % 36n)] + out;
    n /= 36n;
  }
  return out;
}

export function base36ToUuid(encoded: string): string {
  if (encoded.length !== 25) {
    throw new Error(`expected 25 base36 chars, got ${encoded.length}`);
  }

  let n = 0n;
  for (const ch of encoded) {
    const digit = BASE36.indexOf(ch);
    if (digit < 0) {
      throw new Error(`invalid base36 character: ${ch}`);
    }
    n = n * 36n + BigInt(digit);
  }

  if (n > ((1n << 128n) - 1n)) {
    throw new Error("base36 overflow");
  }
  return bigIntToUuid(n);
}

export function typedIdFromUuid(prefix: string, uuid: string): string {
  assertPrefix(prefix);
  return `${prefix}_${uuidToBase36(uuid)}`;
}

export function parseTypedId(id: string, expectedPrefix?: string): ParsedTypedId {
  const match = TYPED_ID_RE.exec(id);
  if (!match) {
    throw new Error(`invalid typed-id: ${id}`);
  }
  const [, prefix, encoded] = match;
  if (expectedPrefix !== undefined && prefix !== expectedPrefix) {
    throw new Error(`expected prefix '${expectedPrefix}', got '${prefix}'`);
  }
  return { prefix, encoded, uuid: base36ToUuid(encoded) };
}

export function isTypedId(id: string, expectedPrefix?: string): boolean {
  try {
    parseTypedId(id, expectedPrefix);
    return true;
  } catch {
    return false;
  }
}

export function retagTypedId(id: string, prefix: string): string {
  assertPrefix(prefix);
  return typedIdFromUuid(prefix, parseTypedId(id).uuid);
}

function fnv1a64(input: string, seed: bigint): bigint {
  let hash = seed;
  for (let i = 0; i < input.length; i++) {
    hash ^= BigInt(input.charCodeAt(i));
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash;
}

export function uuidFromStableSeed(seed: string): string {
  const hi = fnv1a64(seed, 0xcbf29ce484222325n);
  const lo = fnv1a64(`zeroship:${seed}`, 0x84222325cbf29ce4n);
  const bytes = new Uint8Array(16);
  let n = (hi << 64n) | lo;
  for (let i = 15; i >= 0; i--) {
    bytes[i] = Number(n & 0xffn);
    n >>= 8n;
  }

  // Shape the deterministic value as an RFC 4122 UUID with version 7
  // and the standard variant. This is for stable namespace derivation,
  // not timestamp ordering.
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;

  const hex = [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20),
  ].join("-");
}

export function typedIdFromStableSeed(prefix: string, seed: string): string {
  assertPrefix(prefix);
  return typedIdFromUuid(prefix, uuidFromStableSeed(seed));
}
