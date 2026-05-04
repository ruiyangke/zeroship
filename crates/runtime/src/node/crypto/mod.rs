//! Native `node:crypto`.
//!
//! Per `docs/proposals/node-crypto-native.md`. The Stage 1 plan ships
//! Hash / Hmac / random / KDFs / timingSafeEqual / WebCrypto bridge in
//! Stage B; KeyObject + Sign / Verify + Cipher / Decipher in Stage C.
//!
//! # Module layout
//!
//! - `buffer` — Buffer / Uint8Array / DataView / ArrayBuffer / string
//!   input coercion + Buffer-shaped output emission (D-N7).
//! - `encoding` — utf8 / hex / base64 / base64url / latin1 / binary /
//!   ascii / utf16le named-encoding registry.
//! - `hash` — `Hash` class + `createHash(name, options?)` factory.
//! - `hmac` — `Hmac` class + `createHmac(name, key, options?)`.
//! - `random` — `randomBytes`, `randomFillSync`, `randomInt`,
//!   `randomUUID`, `getRandomValues`.
//! - `kdf` — `pbkdf2Sync`, `hkdfSync` (sync; async variants land in a
//!   follow-up using the macro's `#[v8_async_method]` shape).
//! - `misc` — `timingSafeEqual`, `getHashes`, `getCiphers` (stub),
//!   `getCurves` (stub), `getFips`, `setFips`.
//! - `module` — synthetic `node:crypto` ESM module installer; binds
//!   `globalThis.__zeroship_node_crypto.{...}`.
//!
//! The single Rust↔JS boundary object is `globalThis.__zeroship_node_crypto`
//! — the Vite-side synthetic module re-exports each named slot from
//! it. Per D-N26.

pub mod buffer;
pub mod encoding;
pub mod hash;
pub mod hmac;
pub mod kdf;
pub mod key_object;
pub mod misc;
pub mod module;
pub mod random;
pub mod random_callback_helpers;
pub mod sign_verify;
pub mod cipher;
pub mod keygen;
pub mod pkcs8_enc;

pub use module::install_globals;
