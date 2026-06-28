//! Write admission, egress accounting, and socket quota handling.

use std::io;

use crate::state::SharedState;

#[cfg(feature = "runtime_tls")]
use super::driver::TlsOptions;
use super::driver::WriteCmd;
#[cfg(feature = "runtime_tls")]
use super::registry::pending_data_events;
use super::registry::{
    lookup_native_socket_state, push_error_and_close, push_event, SocketEvent,
};

pub(crate) const HIGH_WATER_MARK: u64 = 16 * 1024;
const OUTBOUND_HARD_CAP: u64 = 1024 * 1024;
const OUTBOUND_QUEUE_CAP: usize = 127;

pub fn reset_dispatch_egress(state: &SharedState) {
    let mut s = state.borrow_mut();
    s.native_net_egress_bytes = 0;
    s.native_net_egress_exhausted = false;
}

pub fn set_no_delay(state: &SharedState, socket_id: u32, on: bool) -> std::io::Result<()> {
    let mut tx = {
        let Some(socket) = lookup_native_socket_state(state, socket_id) else {
            return Ok(());
        };
        let mut s = socket.borrow_mut();
        s.pending_no_delay = Some(on);
        s.write_tx.clone()
    };
    if let Some(ref mut tx) = tx {
        tx.try_send(WriteCmd::SetNoDelay(on)).map_err(|e| {
            io::Error::new(io::ErrorKind::BrokenPipe, format!("control queue closed: {e:?}"))
        })?;
    }
    Ok(())
}

pub fn set_keep_alive(
    state: &SharedState,
    socket_id: u32,
    on: bool,
    initial_delay_ms: u64,
) -> std::io::Result<()> {
    let mut tx = {
        let Some(socket) = lookup_native_socket_state(state, socket_id) else {
            return Ok(());
        };
        let mut s = socket.borrow_mut();
        s.pending_keep_alive = Some((on, initial_delay_ms));
        s.write_tx.clone()
    };
    if let Some(ref mut tx) = tx {
        tx.try_send(WriteCmd::SetKeepAlive(on, initial_delay_ms))
            .map_err(|e| {
                io::Error::new(io::ErrorKind::BrokenPipe, format!("control queue closed: {e:?}"))
            })?;
    }
    Ok(())
}

pub fn queue_write(
    state: &SharedState,
    socket_id: u32,
    bytes: Vec<u8>,
) -> Result<bool, String> {
    let n = bytes.len() as u64;
    let (tx, over_hwm) = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let mut sock = socket.borrow_mut();
        if sock.destroyed || sock.ended {
            return Err("write after end".to_string());
        }
        if state.borrow().native_net_egress_exhausted {
            drop(sock);
            push_error_and_close(
                state,
                socket_id,
                "node:net egress ceiling exceeded".to_string(),
                "ERR_NET_EGRESS_CAP",
            );
            return Ok(false);
        }
        let next_buffered = sock.buffered_amount.saturating_add(n);
        if next_buffered > OUTBOUND_HARD_CAP {
            drop(sock);
            push_error_and_close(
                state,
                socket_id,
                "node:net outbound buffer hard cap exceeded".to_string(),
                "ERR_NET_WRITE_CAP",
            );
            return Ok(false);
        }
        let ceiling = { state.borrow().net_policy.egress_ceiling_bytes() };
        if let Some(ceiling) = ceiling {
            let next_socket = sock.egress_total.saturating_add(n);
            let next_app = { state.borrow().native_net_egress_bytes.saturating_add(n) };
            if next_socket > ceiling || next_app > ceiling {
                state.borrow_mut().native_net_egress_exhausted = true;
                drop(sock);
                push_error_and_close(
                    state,
                    socket_id,
                    "node:net egress ceiling exceeded".to_string(),
                    "ERR_NET_EGRESS_CAP",
                );
                return Ok(false);
            }
        }
        sock.egress_total = sock.egress_total.saturating_add(n);
        sock.buffered_amount = next_buffered;
        let over_hwm = sock.buffered_amount >= HIGH_WATER_MARK;
        if over_hwm {
            sock.drain_pending = true;
        }
        if let Some(tx) = sock.write_tx.clone() {
            if let Some(ceiling) = ceiling {
                let next_app = state.borrow().native_net_egress_bytes.saturating_add(n);
                debug_assert!(next_app <= ceiling);
                state.borrow_mut().native_net_egress_bytes = next_app;
            }
            (Some(tx), over_hwm)
        } else if sock.connecting {
            if sock.pending_writes.len() >= OUTBOUND_QUEUE_CAP {
                sock.egress_total = sock.egress_total.saturating_sub(n);
                sock.buffered_amount = sock.buffered_amount.saturating_sub(n);
                sock.drain_pending = sock.buffered_amount >= HIGH_WATER_MARK;
                drop(sock);
                push_error_and_close(
                    state,
                    socket_id,
                    "node:net outbound write queue hard cap exceeded".to_string(),
                    "ERR_NET_WRITE_CAP",
                );
                return Ok(false);
            }
            if let Some(ceiling) = ceiling {
                let next_app = state.borrow().native_net_egress_bytes.saturating_add(n);
                debug_assert!(next_app <= ceiling);
                state.borrow_mut().native_net_egress_bytes = next_app;
            }
            sock.pending_writes.push_back(bytes);
            record_net_egress(state, n);
            return Ok(!over_hwm);
        } else {
            sock.egress_total = sock.egress_total.saturating_sub(n);
            sock.buffered_amount = sock.buffered_amount.saturating_sub(n);
            return Err("Socket is not connected".to_string());
        }
    };

    let Some(mut tx) = tx else {
        return Err("Socket is not connected".to_string());
    };
    match tx.try_send(WriteCmd::Data(bytes)) {
        Ok(()) => {
            record_net_egress(state, n);
            Ok(!over_hwm)
        }
        Err(e) => {
            decrement_buffered_amount(state, socket_id, n);
            rollback_egress(state, socket_id, n);
            push_error_and_close(
                state,
                socket_id,
                format!("node:net outbound write queue refused data: {e:?}"),
                "ERR_NET_WRITE_CAP",
            );
            Ok(false)
        }
    }
}

pub fn queue_end(state: &SharedState, socket_id: u32) -> Result<(), String> {
    let mut tx = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let mut s = socket.borrow_mut();
        s.ended = true;
        if let Some(tx) = s.write_tx.clone() {
            tx
        } else if s.connecting {
            s.pending_end = true;
            return Ok(());
        } else {
            return Err("Socket is not connected".to_string());
        }
    };
    tx.try_send(WriteCmd::End)
        .map_err(|e| format!("end queue closed: {e:?}"))
}

#[cfg(feature = "runtime_tls")]
pub fn queue_start_tls(
    state: &SharedState,
    socket_id: u32,
    opts: TlsOptions,
) -> Result<(), String> {
    if pending_data_events(state, socket_id) {
        push_error_and_close(
            state,
            socket_id,
            "STARTTLS upgrade refused with pending plaintext bytes".to_string(),
            "ERR_TLS_HANDSHAKE",
        );
        return Ok(());
    }
    let mut tx = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let s = socket.borrow();
        if s.destroyed {
            return Err("Socket is closed".to_string());
        }
        if !s.connected {
            return Err("Socket is not connected".to_string());
        }
        if s.encrypted {
            return Err("Socket is already encrypted".to_string());
        }
        s.write_tx
            .clone()
            .ok_or_else(|| "Socket is not connected".to_string())?
    };
    tx.try_send(WriteCmd::StartTls(opts))
        .map_err(|e| format!("TLS upgrade queue closed: {e:?}"))
}

pub(super) fn decrement_buffered_amount(state: &SharedState, socket_id: u32, n: u64) {
    let should_drain = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        s.buffered_amount = s.buffered_amount.saturating_sub(n);
        if s.drain_pending && s.buffered_amount < HIGH_WATER_MARK {
            s.drain_pending = false;
            true
        } else {
            false
        }
    } else {
        false
    };
    if should_drain {
        push_event(state, socket_id, SocketEvent::Drain);
    }
}

fn rollback_egress(state: &SharedState, socket_id: u32, n: u64) {
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut socket = socket.borrow_mut();
        socket.egress_total = socket.egress_total.saturating_sub(n);
    }
    let mut s = state.borrow_mut();
    s.native_net_egress_bytes = s.native_net_egress_bytes.saturating_sub(n);
}

fn record_net_egress(state: &SharedState, n: u64) {
    let meter = { state.borrow().meter.clone() };
    if let Some(meter) = meter {
        meter.record("egress_bytes", n);
        meter.record("net_egress_bytes", n);
    }
}

pub(super) fn record_net_ingress(state: &SharedState, n: u64) {
    let meter = { state.borrow().meter.clone() };
    if let Some(meter) = meter {
        meter.record("ingress_bytes", n);
        meter.record("net_ingress_bytes", n);
    }
}

pub fn reserve_socket_slot(state: &SharedState, socket_id: u32) -> Result<(), String> {
    let max = state.borrow().net_policy.max_sockets();
    if max == 0 {
        return Err("node:net capability denied".to_string());
    }
    {
        let s = state.borrow();
        if s.native_net_egress_exhausted {
            return Err("node:net egress ceiling exceeded".to_string());
        }
        if s.active_native_sockets >= max {
            return Err(format!("per-app node:net socket cap exceeded ({max})"));
        }
    }
    crate::transport::net_policy::try_acquire_global_socket()?;
    {
        let mut s = state.borrow_mut();
        s.active_native_sockets += 1;
    }
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut sock = socket.borrow_mut();
        sock.counted = true;
        sock.connecting = true;
    }
    Ok(())
}

pub fn release_socket_slot(state: &SharedState, socket_id: u32) {
    let counted = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        if s.counted {
            s.counted = false;
            true
        } else {
            false
        }
    } else {
        false
    };
    if counted {
        {
            let mut s = state.borrow_mut();
            s.active_native_sockets = s.active_native_sockets.saturating_sub(1);
        }
        crate::transport::net_policy::release_global_socket();
    }
}
