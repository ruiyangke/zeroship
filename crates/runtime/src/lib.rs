#![allow(unsafe_code)]

pub mod state;
pub mod dispatch;
pub mod init;
pub mod http;
pub mod fetch;
pub mod modules;
pub mod crypto;
pub mod streams;
pub mod url;
pub mod channel;
pub mod runtime;
pub mod storage;
pub mod bundle;
pub mod serve;

#[cfg(target_os = "linux")]
pub mod cpu_timer;

// Convenience re-exports
pub use init::{init_v8, RequestResult, HttpResult};
pub use modules::ModuleEntry;
pub use state::{SharedState, RuntimeState, OpResult, FetchRequest, StreamState, SpawnedTimer};
pub use storage::AppStorage;
pub use runtime::{Runtime, DispatchOutcome, HttpDispatchResult, AsyncWork, AsyncEvent};
pub use channel::{ResultSender, ResultReceiver, StreamWriter, StreamReader};
pub use bundle::{AppBundle, ModuleType, ModuleInfo};
pub use serve::{start_server, ServerOptions};
