//! Compile-fail doctests for the `#[v8_async_method]` rejection rules.
//! These live at the runtime crate level (not on the proc macro itself)
//! because proc-macro crates can't depend on themselves, so doctests
//! that exercise the macro must run from a downstream crate.
//!
//! `&mut self` is rejected — borrow across `.await` is unsound under
//! V8 re-entry:
//!
//! ```compile_fail
//! use zeroship_runtime::state::OpError;
//! use zeroship_runtime_macros::{v8_class, v8_async_method, v8_constructor};
//! struct Mutator { n: u32 }
//! #[v8_class]
//! impl Mutator {
//!     #[v8_constructor]
//!     fn new() -> Self { Mutator { n: 0 } }
//!     #[v8_async_method]
//!     async fn bump(&mut self) -> Result<u32, OpError> {
//!         self.n += 1;
//!         Ok(self.n)
//!     }
//! }
//! ```
//!
//! Non-`async` methods marked with the attribute are rejected:
//!
//! ```compile_fail
//! use zeroship_runtime::state::OpError;
//! use zeroship_runtime_macros::{v8_class, v8_async_method, v8_constructor};
//! struct NotAsync;
//! #[v8_class]
//! impl NotAsync {
//!     #[v8_constructor]
//!     fn new() -> Self { NotAsync }
//!     #[v8_async_method]
//!     fn must_be_async(&self) -> Result<u32, OpError> { Ok(0) }
//! }
//! ```
//!
//! The positive `&self` shape compiles fine:
//!
//! ```
//! use std::cell::Cell;
//! use zeroship_runtime::state::OpError;
//! use zeroship_runtime_macros::{v8_class, v8_async_method, v8_constructor};
//! struct Ok_ { n: Cell<u32> }
//! #[v8_class]
//! impl Ok_ {
//!     #[v8_constructor]
//!     fn new() -> Self { Ok_ { n: Cell::new(0) } }
//!     #[v8_async_method]
//!     async fn bump(&self) -> Result<u32, OpError> {
//!         self.n.set(self.n.get() + 1);
//!         Ok(self.n.get())
//!     }
//! }
//! ```
//!
//! ## `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` rejection rules
//!
//! Fastcall codegen restricts the user's signature to fit V8's fast API
//! constraints. The macro emits compile-fail errors at the right span so
//! mistakes surface as clear messages rather than runtime UB.
//!
//! `&mut self` is rejected — fast-path callbacks have no scope and so
//! the slow path's per-method re-entrancy guard cannot be emitted:
//!
//! ```compile_fail
//! use zeroship_runtime_macros::{v8_class, v8_method, v8_constructor};
//! struct M { n: u32 }
//! #[v8_class]
//! impl M {
//!     #[v8_constructor]
//!     fn new() -> Self { M { n: 0 } }
//!     #[v8_method(fastcall)]
//!     fn bump(&mut self) -> u32 { self.n += 1; self.n }
//! }
//! ```
//!
//! `String` return is rejected — fast path forbids allocation:
//!
//! ```compile_fail
//! use zeroship_runtime_macros::{v8_class, v8_method, v8_constructor};
//! struct M;
//! #[v8_class]
//! impl M {
//!     #[v8_constructor]
//!     fn new() -> Self { M }
//!     #[v8_method(fastcall)]
//!     fn name(&self) -> String { "x".to_string() }
//! }
//! ```
//!
//! `Vec<u8>` return is rejected — same allocation reason:
//!
//! ```compile_fail
//! use zeroship_runtime_macros::{v8_class, v8_method, v8_constructor};
//! struct M;
//! #[v8_class]
//! impl M {
//!     #[v8_constructor]
//!     fn new() -> Self { M }
//!     #[v8_method(fastcall)]
//!     fn bytes(&self) -> Vec<u8> { vec![] }
//! }
//! ```
//!
//! `Option<T>` return is rejected — fast path can't represent `null`:
//!
//! ```compile_fail
//! use zeroship_runtime_macros::{v8_class, v8_method, v8_constructor};
//! struct M;
//! #[v8_class]
//! impl M {
//!     #[v8_constructor]
//!     fn new() -> Self { M }
//!     #[v8_method(fastcall)]
//!     fn opt(&self) -> Option<u32> { None }
//! }
//! ```
//!
//! Positive shapes compile fine:
//!
//! ```
//! use zeroship_runtime::state::OpError;
//! use zeroship_runtime_macros::{v8_class, v8_method, v8_getter, v8_constructor};
//! struct OkF { n: u32 }
//! #[v8_class]
//! impl OkF {
//!     #[v8_constructor]
//!     fn new() -> Self { OkF { n: 0 } }
//!     #[v8_getter(fastcall)]
//!     fn count(&self) -> u32 { self.n }
//!     #[v8_method(fastcall)]
//!     fn ping(&self, n: u32) -> u32 { n + 1 }
//!     #[v8_method(fastcall)]
//!     fn check(&self, n: u32) -> Result<bool, OpError> {
//!         Ok(n > 0)
//!     }
//! }
//! ```

#![allow(unsafe_code)]
#![allow(missing_debug_implementations)]

// Self-rename for use by the `#[v8_class]` proc macro. It emits
// `::zeroship_runtime::state::OpErrorKind` so the path resolves from
// both downstream crates AND from inside the runtime itself.
extern crate self as zeroship_runtime;

pub mod auth;
pub mod base;
pub mod core;
pub use base::crypto as crypto_ops;  // back-compat shim — all existing
                                     // crate::crypto_ops::* paths keep working
pub mod fetch_outcome;
pub mod macro_runtime;  // stable re-export surface for runtime-macros emit
                        // (Wave 5a, design §3.9). DO NOT bypass this from
                        // the macro — see the module-level rustdoc.
pub mod node;
pub mod rpc;
pub mod storage;
pub mod transport;
pub mod web;
pub mod webidl;

// Back-compat re-exports for the SSRF helper (was `crate::fetch`) and the
// kernel HTTP bridge (was `crate::http`). Both moved under `transport/`.
pub use transport::handler as http;
pub use transport::ssrf as fetch;

// Back-compat re-exports for modules now grouped under `web/`.
// External crates (`worker`, `cli`, plugin-*, tests) import via top-level
// paths; the impls live under `web::` but the old paths keep working.
pub use web::base64;
pub use web::blob as blob_native;
pub use web::codec;
pub use web::crypto as crypto_native;
pub use web::crypto_node;
pub use web::dom;
pub use web::encoding as text_encoding;
pub use web::fetch as fetch_native;
pub use web::fetch::body as fetch_body;
pub use web::fetch::request as fetch_request;
pub use web::fetch::response as fetch_response;
pub use web::headers;
pub use web::streams;
pub use web::structured_clone;
pub use web::url as url_native;
pub use web::websocket as websocket_native;

// `crypto.rs` (sync hash/HMAC + fast_random) moved to
// `web::crypto::sync_helpers`. Re-export the module so internal
// `crate::crypto::fast_random` etc. paths still resolve.
pub use web::crypto::sync_helpers as crypto;

// Back-compat re-exports for modules that have moved into `core/`.
// External crates (`worker`, `cli`, plugin-*) import via `zeroship_runtime::state::...`,
// `zeroship_runtime::runtime::...`, etc. The implementations now live in
// `core::` but the old paths keep working.
pub use core::channel;
pub use core::dispatch;
pub use core::init;
pub use core::modules;
pub use core::native_modules;
pub use core::node_error;
pub(crate) use core::panic_util;
pub use core::plugin;
pub use core::runtime;
pub use core::serve;
pub use core::state;

// Back-compat re-exports for the webidl/ types (formerly at root).
pub use webidl::byte_string;
pub use webidl::clamp;
pub use webidl::convert;
pub use webidl::enforce_range;
pub use webidl::usv_string;
pub use webidl::wrap;

#[cfg(target_os = "linux")]
pub use core::cpu_timer;

// Convenience re-exports
pub use clamp::{
    ClampI32, ClampI64, ClampU16, ClampU32, ClampU64,
    read_clamp_i32, read_clamp_i64, read_clamp_u16, read_clamp_u32, read_clamp_u64,
};
pub use enforce_range::{EnforceRangeU32, EnforceRangeU64, read_enforce_range_u32, read_enforce_range_u64};
pub use convert::{DictOrBool, WebIdlConvertible, read_record, read_sequence};
pub use url_native::helpers::USVString;
pub use init::{init_v8, RequestResult, HttpResult};
pub use modules::ModuleEntry;
pub use state::{SharedState, RuntimeState, OpResult, SpawnedTimer, WebSocketState, WsMessage};
pub use storage::AppStorage;
pub use runtime::{Runtime, RuntimeBuilder, RuntimeLimits, AsyncWork, AsyncEvent};
pub use fetch_outcome::{FetchOutcome, SettledFetch, RequestCtx, EnvSnapshot};
pub use channel::{CancelFlag, ResultSender, ResultReceiver, StreamWriter, StreamReader};
pub use serve::{start_server, ServerOptions};
pub use plugin::{NativePlugin, NativeRegistrar};
