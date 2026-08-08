//! Accepted-shape doctests for the `#[v8_async_method]` and
//! `#[v8_method(fastcall)]` attributes.
//!
//! These live at the runtime crate level (not on the proc macro itself)
//! because proc-macro crates can't depend on themselves, so anything
//! exercising the macro must run from a downstream crate.
//!
//! # The REJECTION rules are pinned in `tests/`, not here
//!
//! Six rules used to be pinned by `compile_fail` doctests in this
//! header. They are now trybuild fixtures with `.stderr` snapshots:
//!
//!   tests/v8_async_method_compile_fail.rs  + tests/compile_fail_async_method/
//!   tests/v8_fastcall_compile_fail.rs      + tests/compile_fail_fastcall/
//!
//! A raw `compile_fail` passes on ANY compilation error, so it cannot
//! distinguish the rejection it means to pin from a typo, a renamed
//! macro, or a stale import - the pin keeps passing while the property
//! goes untested, and reads as coverage the whole time. The four
//! fastcall cases made that concrete: they differ only in return type,
//! so one error in the shared scaffolding would have satisfied all four
//! at once. trybuild asserts the diagnostic MATCHES the snapshot, so
//! failing for the wrong reason is a test failure.
//!
//! What remains below is the other half, and doctests are the right
//! tool for it: the shapes that MUST compile. Keep both - a rejection
//! rule with no accepted counterpart can be satisfied by rejecting
//! everything.
//!
//! The accepted `&self` async shape:
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
//! ## `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` accepted shapes
//!
//! Fastcall codegen restricts the user's signature to fit V8's fast API
//! constraints, and the macro rejects the rest at the right span so
//! mistakes surface as clear messages rather than runtime UB. The four
//! rejections - `&mut self`, and `String` / `Vec<u8>` / `Option<T>`
//! returns - are pinned by the fixtures under
//! `tests/compile_fail_fastcall/`, each with its own `.stderr` naming
//! the offending type.
//!
//! The shapes that must keep compiling:
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
                        // (see design §3.9). DO NOT bypass this from
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
pub use transport::net_policy::{HostPort, NetPolicy, ReviewedAllowlist};
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
pub use core::dev_auth;
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
pub use init::{init_v8, init_v8_single_threaded, v8_platform_flavor, RequestResult, HttpResult};
pub use modules::ModuleEntry;
pub use state::{SharedState, RuntimeState, OpResult, SpawnedTimer, WebSocketState, WsMessage};
pub use storage::AppStorage;
pub use runtime::{
    heap_limit_callback_hits, heap_used_and_limit, AsyncEvent, AsyncWork, Runtime, RuntimeBuilder, RuntimeLease,
    RuntimeLimits,
};
pub use fetch_outcome::{
    EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch, SettledWorkflow, WorkflowOutcome,
};
pub use channel::{CancelFlag, ResultSender, ResultReceiver, StreamWriter, StreamReader};
pub use serve::{start_server, ServerOptions};
pub use plugin::{NativePlugin, NativeRegistrar};
