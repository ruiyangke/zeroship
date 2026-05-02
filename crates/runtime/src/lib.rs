#![allow(unsafe_code)]
#![allow(missing_debug_implementations)]

// Self-rename for use by `#[zeroship_op]` and `#[v8_class]` proc macros.
// They emit `::zeroship_runtime::state::OpErrorKind` so the path resolves
// from both downstream crates AND from inside the runtime itself.
extern crate self as zeroship_runtime;

pub mod auth;
pub mod byte_string;
pub mod codec;
pub mod enforce_range;
pub mod headers;
pub mod state;
pub mod text_encoding;
pub mod dispatch;
pub mod init;
pub mod http;
pub mod fetch;
pub mod fetch_outcome;
pub mod modules;
pub mod crypto;
pub mod streams;
pub mod url;
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
pub use enforce_range::{EnforceRangeU64, read_enforce_range_u64};
pub use url_native::helpers::USVString;
pub use init::{init_v8, RequestResult, HttpResult};
pub use modules::ModuleEntry;
pub use state::{SharedState, RuntimeState, OpResult, FetchRequest, StreamState, SpawnedTimer, WebSocketState, WsMessage};
pub use storage::AppStorage;
pub use runtime::{Runtime, RuntimeBuilder, RuntimeLimits, AsyncWork, AsyncEvent};
pub use fetch_outcome::{FetchOutcome, SettledFetch, RequestCtx, EnvSnapshot};
pub use channel::{CancelFlag, ResultSender, ResultReceiver, StreamWriter, StreamReader};
pub use serve::{start_server, ServerOptions};
pub use plugin::{NativePlugin, NativeRegistrar};
