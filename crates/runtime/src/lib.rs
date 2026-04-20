#![allow(unsafe_code)]
#![allow(missing_debug_implementations)]

pub mod auth;
pub mod state;
pub mod dispatch;
pub mod init;
pub mod http;
pub mod fetch;
pub mod fetch_outcome;
pub mod modules;
pub mod crypto;
pub mod streams;
pub mod url;
pub mod websocket;
pub mod channel;
pub mod runtime;
pub mod storage;
pub mod bundle;
pub mod serve;
pub mod plugin;
pub(crate) mod panic_util;

#[cfg(target_os = "linux")]
pub mod cpu_timer;

// Convenience re-exports
pub use init::{init_v8, RequestResult, HttpResult};
pub use modules::ModuleEntry;
pub use state::{SharedState, RuntimeState, OpResult, FetchRequest, StreamState, SpawnedTimer, WebSocketState, WsMessage};
pub use storage::AppStorage;
pub use runtime::{Runtime, RuntimeBuilder, RuntimeLimits, DispatchOutcome, HttpDispatchResult, AsyncWork, AsyncEvent};
pub use fetch_outcome::{FetchOutcome, SettledFetch, RequestCtx, EnvSnapshot};
pub use channel::{CancelFlag, ResultSender, ResultReceiver, StreamWriter, StreamReader};
pub use bundle::{AppBundle, ModuleType, ModuleInfo};
pub use serve::{start_server, ServerOptions};
pub use plugin::{NativePlugin, NativeRegistrar};
