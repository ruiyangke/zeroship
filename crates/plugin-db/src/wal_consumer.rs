//! Streaming WAL consumer — DEFERRED to P8a.2.
//!
//! This module is the placeholder for the per-worker pgoutput consumer
//! thread described in the proposal §C1 step 3. **It is intentionally
//! a stub for P8a** because compio-postgres does not yet speak the
//! Postgres streaming-replication protocol.
//!
//! ## The blocker
//!
//! Streaming logical replication requires the client to:
//!
//! 1. Connect with `replication=database` in the startup parameters.
//!    This puts Postgres into "walsender" mode where the regular SQL
//!    grammar is replaced by a small set of replication commands.
//! 2. Issue `START_REPLICATION SLOT <name> LOGICAL <lsn>
//!    ("proto_version" '4', "publication_names" '<pub>')`. Postgres
//!    responds with `CopyBothResponse` and then streams `XLogData`
//!    and `PrimaryKeepaliveMessage` frames over a CopyBoth channel.
//! 3. Send periodic `StandbyStatusUpdate` frames to advance the
//!    slot's `confirmed_flush_lsn` (essential — without this, WAL
//!    retention grows unbounded).
//!
//! compio-postgres (the bespoke compio/io_uring driver this codebase
//! uses) implements the regular query protocol only. It has no
//! `replication` config knob, no `START_REPLICATION` encoder, and no
//! `XLogData`/`CopyBoth` decoder. Adding them is a ~1500-LOC change
//! across `config.rs`, `connect_raw.rs`, `codec.rs`, and a new
//! `replication.rs` — well-scoped but bigger than fits inside the
//! P8a budget.
//!
//! Bypassing the streaming protocol via `pg_logical_slot_get_binary_changes()`
//! polled from a regular connection is *possible* — the function
//! returns pgoutput frames as BYTEA rows. It is **not** what we ship
//! because (a) the polling cadence trades latency for CPU in a way
//! that makes the SLO unreproducible, and (b) every poll moves the
//! slot's `confirmed_flush_lsn` regardless of whether the broker has
//! actually fanned the events out, defeating the slot's safety
//! guarantee on broker crash. The streaming protocol with explicit
//! `StandbyStatusUpdate` is the only correct path.
//!
//! ## The P8a fallback path
//!
//! Until P8a.2 lands, the broker is fed events by the mutation
//! callbacks themselves (`insert`, `updateOne`, `deleteOne`,
//! `upsert`, etc.) on the SAME isolate thread. This means:
//!
//! - Subscribers in the **same worker as the writer** see events
//!   correctly. Tests that drive both sides through V8 callbacks pass.
//! - Subscribers in a **different worker** see nothing. The platform
//!   today routes per-app traffic to a single worker via CHWBL, so the
//!   cross-worker case is rare — but it WILL fire on worker scale-out
//!   and on any deploy with a multi-tenant pinning miss. P8a documents
//!   this as a known gap (`docs/runbooks/reactive-queries.md`, to be
//!   written alongside the cross-worker WAL path).
//!
//! ## What lands when P8a.2 ships
//!
//! The streaming consumer becomes a long-running compio task spawned
//! per-app on demand:
//!
//! ```ignore
//! // pseudocode
//! pub async fn run_consumer(app_id: &str) {
//!     let conn = compio_postgres::connect_replication(url).await?;
//!     conn.start_replication(slot, last_lsn, &[publication]).await?;
//!     while let Some(frame) = conn.next_xlogdata().await {
//!         for event in pgoutput::decode(frame)? {
//!             broker::publish(&event);
//!         }
//!         conn.send_standby_status(last_consumed_lsn).await?;
//!     }
//! }
//! ```
//!
//! The broker's API does not change between P8a and P8a.2 — the
//! consumer feeds `broker::publish` the same `ChangeEvent` shape the
//! local-emit path constructs today. Subscribers don't need to know
//! the difference.
//!
//! ## P8b notes (read-set filtering)
//!
//! When P8b lands, the consumer's `decode` step also extracts the
//! `new_tuple_excerpt` (indexed columns only, capped at ~1 KB per
//! event per the proposal). That excerpt feeds the read-set
//! fingerprint matcher: an event for `messages` with `channel_id=42`
//! only wakes subscribers whose ReadSet includes `(messages,
//! channel_id, =, 42 | *)`. For P8a we don't extract column values
//! because the broker only matches on `(app_id, collection)` — adding
//! them now would be dead weight.

use crate::broker::{publish, ChangeEvent, ChangeOp};

/// Emit a local change event into the in-process broker.
///
/// Called from the mutation callbacks (`insert`, `update_one`,
/// `delete_one`, ...) after a successful SQL run. The
/// `pk` argument is the integer surrogate id of the affected row,
/// or `None` if the operation didn't return one (DELETE with no
/// RETURNING).
///
/// ## Why this is in `wal_consumer.rs` and not `broker.rs`
///
/// Two reasons:
/// 1. The mutation callbacks shouldn't depend directly on broker
///    internals — when the streaming consumer takes over event
///    emission (P8a.2), this function disappears and the callbacks
///    stop emitting locally. Keeping the function in
///    `wal_consumer.rs` makes the "deprecation site" obvious.
/// 2. Test cleanup that wants to disable local emission (e.g. to
///    reproduce the cross-worker-only state) can be added here
///    behind a thread-local toggle without touching the broker.
pub fn emit_local(
    app_id: &str,
    collection: &str,
    op: ChangeOp,
    pk: Option<i64>,
    changed_columns: Vec<String>,
) {
    publish(&ChangeEvent {
        app_id: app_id.to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{Broker, SubscriptionMessage};

    #[test]
    fn emit_local_reaches_thread_local_broker() {
        // The thread-local broker is shared across tests on the same
        // thread; clean it before observing.
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("xapp", "messages");
        emit_local("xapp", "messages", ChangeOp::Insert, Some(99), vec!["title".into()]);
        match sub.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.app_id, "xapp");
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk, Some(99));
                assert_eq!(ev.changed_columns, vec!["title".to_string()]);
            }
            other => panic!("expected Change variant, got {other:?}"),
        }
        // Cleanup so subsequent tests on the same thread see a clean broker.
        crate::broker::drop_app(None);
    }

    #[test]
    fn emit_local_with_no_subscriber_is_noop() {
        // Broker has no subscriber for this (app, collection) — must
        // not panic or allocate user-visible state.
        let _ = Broker::new(); // proves construction works
        emit_local("nobody", "ghosts", ChangeOp::Delete, None, vec![]);
    }
}
