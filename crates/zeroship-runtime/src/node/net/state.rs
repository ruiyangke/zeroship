//! Compatibility re-exports for native `node:net.Socket` state.
//!
//! The implementation is split by responsibility:
//! - `registry` owns ids, wrappers, event queues, and lifecycle transitions.
//! - `caps` owns write admission, egress accounting, and socket quotas.
//! - `connect` owns DNS/TCP/TLS connect tasks.
//! - `driver` owns established-stream byte-pump consumers and STARTTLS.

pub use super::caps::{
    queue_end, queue_write, reserve_socket_slot, reset_dispatch_egress, set_keep_alive,
    set_no_delay,
};
#[cfg(feature = "runtime_tls")]
pub use super::caps::queue_start_tls;
#[cfg(feature = "runtime_tls")]
pub use super::driver::TlsOptions;
pub use super::driver::WriteCmd;
pub use super::registry::{
    NativeSocketState, SocketEvent, alloc_native_socket_id, attach_wrapper, destroy_all_sockets,
    destroy_socket, drain_events, free_native_socket_state, lookup_native_socket_state,
    pause_socket, requeue_front_events, resume_socket, socket_paused,
};
