//! Native `node:crypto`.
//!
//! See `docs/proposals/node-crypto-native.md`. The current module ships
//! Hash / Hmac / random / KDFs / timingSafeEqual / the WebCrypto bridge,
//! plus KeyObject, Sign / Verify, and Cipher / Decipher.
//!
//! # Module layout
//!
//! - `buffer` — Buffer / Uint8Array / DataView / ArrayBuffer / string
//!   input coercion + Buffer-shaped output emission.
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
//! - `module` — synthetic `node:crypto` ESM module: V8
//!   `SyntheticModule` whose exports are populated lazily on import.
//!
//! User code imports the module directly:
//!
//! ```js
//! import { createHash, randomUUID } from "node:crypto";
//! ```
//!
//! Resolved by `core::native_modules::resolve_native`.

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

pub use module::{populate, synthetic_module};
