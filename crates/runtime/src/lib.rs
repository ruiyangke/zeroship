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

#![allow(unsafe_code)]
#![allow(missing_debug_implementations)]

// Self-rename for use by `#[zeroship_op]` and `#[v8_class]` proc macros.
// They emit `::zeroship_runtime::state::OpErrorKind` so the path resolves
// from both downstream crates AND from inside the runtime itself.
extern crate self as zeroship_runtime;

pub mod auth;
pub mod base64;
pub mod blob_native;
pub mod byte_string;
pub mod clamp;
pub mod codec;
pub mod dom;
pub mod enforce_range;
pub mod fetch_body;
pub mod fetch_request;
pub mod fetch_response;
pub mod headers;
pub mod state;
pub mod text_encoding;
pub mod dispatch;
pub mod init;
pub mod http;
pub mod fetch;
pub mod fetch_native;
pub mod fetch_outcome;
pub mod modules;
pub mod crypto;
pub mod crypto_native;
pub mod streams;
pub mod structured_clone;
pub mod url_native;
pub mod websocket;
pub mod channel;
pub mod runtime;
pub mod storage;
pub mod serve;
pub mod plugin;
pub(crate) mod panic_util;

#[cfg(target_os = "linux")]
pub mod cpu_timer;

// Convenience re-exports
pub use clamp::{
    ClampI32, ClampI64, ClampU16, ClampU32, ClampU64,
    read_clamp_i32, read_clamp_i64, read_clamp_u16, read_clamp_u32, read_clamp_u64,
};
pub use enforce_range::{EnforceRangeU32, EnforceRangeU64, read_enforce_range_u32, read_enforce_range_u64};
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
