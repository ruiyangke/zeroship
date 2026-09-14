/**
 * PKCE (RFC 7636) + state/nonce minting via Web Crypto.
 *
 * The browser holds the verifier; only the S256 challenge is sent to the
 * gateway's `GET /__zeroship/auth/authorize`. The verifier travels to
 * `POST /__zeroship/auth/session` (same-origin proxy) at exchange time and never
 * leaves first-party storage otherwise (gateway §1.2).
 */

import type { CryptoLike } from "./env";

/** RFC 7636 unreserved alphabet for the code verifier. */
const VERIFIER_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/**
 * RFC 4648 §5 base64url alphabet (`-`/`_` in place of `+`/`/`). Using it
 * directly in the encode loop means the output is base64url with no post-pass
 * character fixup and no padding.
 */
const B64URL_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/**
 * base64url (no padding) of raw bytes. A self-contained encoder (no `btoa`,
 * no `Buffer`) so this module is pure browser/worker code that needs neither
 * the DOM `btoa` global nor Node's `Buffer` — it runs identically everywhere.
 */
function base64UrlEncode(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i];
    const b1 = i + 1 < bytes.length ? bytes[i + 1] : 0;
    const b2 = i + 2 < bytes.length ? bytes[i + 2] : 0;
    out += B64URL_ALPHABET[b0 >> 2];
    out += B64URL_ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)];
    if (i + 1 < bytes.length) out += B64URL_ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)];
    if (i + 2 < bytes.length) out += B64URL_ALPHABET[b2 & 0x3f];
  }
  return out;
}

/** A high-entropy random string drawn from the PKCE unreserved alphabet. */
function randomString(crypto: CryptoLike, length: number): string {
  const bytes = new Uint8Array(length);
  crypto.getRandomValues(bytes);
  let out = "";
  for (const b of bytes) out += VERIFIER_CHARS[b % VERIFIER_CHARS.length];
  return out;
}

/** Generate a PKCE code verifier (43–128 chars; we use 64). */
export function generateVerifier(crypto: CryptoLike): string {
  return randomString(crypto, 64);
}

/** Derive the S256 challenge = base64url(SHA-256(verifier)). */
export async function s256Challenge(crypto: CryptoLike, verifier: string): Promise<string> {
  const data = new TextEncoder().encode(verifier);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return base64UrlEncode(new Uint8Array(digest));
}

/** A short opaque random token for `state` / `nonce`. */
export function randomToken(crypto: CryptoLike, length = 32): string {
  return randomString(crypto, length);
}

/** Everything a single authorize→exchange flow needs to remember. */
export interface PkceParams {
  verifier: string;
  challenge: string;
  state: string;
  nonce: string;
}

/** Mint a fresh PKCE+state+nonce tuple for one popup/redirect flow. */
export async function generatePkce(crypto: CryptoLike): Promise<PkceParams> {
  const verifier = generateVerifier(crypto);
  const challenge = await s256Challenge(crypto, verifier);
  return {
    verifier,
    challenge,
    state: randomToken(crypto),
    nonce: randomToken(crypto),
  };
}
