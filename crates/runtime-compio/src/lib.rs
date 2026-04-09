#![allow(unsafe_code)]

pub mod runtime;
pub mod fetch;

// Re-export v8-core for convenience
pub use appbase_v8_core as v8_core;
pub use appbase_v8_core::state;
pub use appbase_v8_core::init;
pub use appbase_v8_core::modules;

pub use appbase_v8_core::{init_v8, RequestResult, ModuleEntry,
    IncomingRequest, RequestReply, RequestKind, OpResult, FetchRequest};
pub use runtime::Runtime;
