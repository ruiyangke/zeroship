#![allow(unsafe_code)]

pub mod v8;
pub mod io;

// Convenience re-exports (match the old appbase-runtime-compio public API)
pub use v8::init::{init_v8, RequestResult, HttpResult};
pub use v8::modules::ModuleEntry;
pub use v8::state::{SharedState, RuntimeState, OpResult, FetchRequest, StreamState, SpawnedTimer};
pub use v8::storage::AppStorage;
pub use io::runtime::{Runtime, DispatchOutcome, HttpDispatchResult, AsyncWork, AsyncEvent};
pub use io::channel::{ResultSender, ResultReceiver, StreamWriter, StreamReader};

// Re-export sub-modules at top level for backward compat with platform crate
pub mod modules {
    pub use crate::v8::modules::*;
}
pub mod runtime {
    pub use crate::io::runtime::*;
}
pub mod state {
    pub use crate::v8::state::*;
}
pub mod init {
    pub use crate::v8::init::*;
}

