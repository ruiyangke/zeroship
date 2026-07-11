//! The host-callback [`PgSession`] bridge (design §B + §B.6).
//!
//! [`NapiHostSession`] implements [`PgSession`] by marshaling each verb to a host
//! *verb dispatcher* and awaiting the reply on a `futures::channel::oneshot`. The
//! dispatcher is abstracted behind [`VerbDispatch`] so:
//!
//! - the **napi transport** ([`crate::bridge::TsfnDispatch`], `napi` feature)
//!   fires a `ThreadsafeFunction` at the JS host driver and resolves the oneshot
//!   from the Rust-supplied `done` callback (§B.3);
//! - a **mock dispatcher** (the integration test) returns canned rows synchronously
//!   and asserts the recorded SQL sequence — proving the bridge drives a real apply
//!   without a Node host.
//!
//! Either way, [`NapiHostSession`] carries the **one-in-flight `AtomicBool` guard**
//! (§B.6): each verb `compare_exchange(false, true)`s on entry and clears on the
//! completion arm (RAII), panicking on re-entry. On a real pinned host connection a
//! second concurrent verb would deadlock (its `tsfn.call` blocks on a socket the
//! first hasn't released); the guard converts that into a loud panic. Only
//! `Send + 'static` data crosses the dispatcher boundary (§B.3): the owned
//! [`JsRequest`](crate::marshal::JsRequest) payload out, and an owned
//! `Result<Vec<JsRow>, JsError>` back.

use std::sync::atomic::{AtomicBool, Ordering};

use zeroship_migrate::apply::backend::postgres::seam::{SeamBind, SeamError, SeamRow};
use zeroship_migrate::apply::backend::postgres::session::PgSession;

use crate::marshal::{
    bind_to_cell, js_error_to_seam, row_to_seam, JsError, JsReply, JsRequest,
};

/// The verb kinds crossing to the host (kept in sync with [`JsRequest::kind`]).
pub const KIND_BATCH: &str = "batch";
pub const KIND_EXECUTE: &str = "execute";
pub const KIND_EXECUTE_TEXT: &str = "executeTextParams";
pub const KIND_QUERY: &str = "query";
pub const KIND_QUERY_ONE: &str = "queryOne";

/// The reply the host returns for a verb: either the rows + affected count, or a
/// neutral error. `Send + 'static` (plain owned data) so it may cross back from the
/// JS thread through the `done` callback (§B.3).
pub type VerbReply = Result<JsReply, JsError>;

/// The host-side transport [`NapiHostSession`] dispatches each verb through.
///
/// `dispatch` takes an owned [`JsRequest`] (the only value crossing out) and must
/// eventually produce a [`VerbReply`]. The napi impl fires a `ThreadsafeFunction`
/// and parks the engine future on a oneshot the host's `done` callback fires; the
/// mock impl answers inline. `dispatch` is `async` in the trait but its body never
/// awaits a JS `Promise` — only a `oneshot::Receiver` (§B.3), so the engine's
/// reactor-less `block_on` stays sufficient.
#[allow(async_fn_in_trait)] // !Send single-thread engine, by design (mirrors PgSession)
pub trait VerbDispatch {
    /// Send one verb request to the host and await its reply.
    async fn dispatch(&self, req: JsRequest) -> VerbReply;
}

/// A host-driven [`PgSession`] over an abstract [`VerbDispatch`] transport.
pub struct NapiHostSession<D: VerbDispatch> {
    dispatch: D,
    /// The mechanically-enforced one-verb-at-a-time guard (§B.6).
    in_flight: AtomicBool,
}

impl<D: VerbDispatch> NapiHostSession<D> {
    /// Wrap a transport into a `PgSession`.
    pub fn new(dispatch: D) -> Self {
        Self {
            dispatch,
            in_flight: AtomicBool::new(false),
        }
    }

    /// Consume the session and return the underlying transport (for a mock/test to
    /// read back its recording after an apply — the transport is otherwise fully
    /// owned by the session).
    pub fn into_dispatch(self) -> D {
        self.dispatch
    }

    /// RAII arm of the one-in-flight guard (§B.6). Panics on re-entry.
    fn enter(&self) -> InFlightGuard<'_> {
        InFlightGuard::enter(&self.in_flight)
    }

    /// Marshal binds and run a verb, returning the full [`JsReply`] (rows + count).
    async fn call(&self, kind: &str, sql: &str, binds: &[SeamBind]) -> Result<JsReply, SeamError> {
        let _g = self.enter();
        let req = JsRequest {
            kind: kind.to_string(),
            sql: sql.to_string(),
            binds: binds.iter().map(bind_to_cell).collect(),
            text_params: Vec::new(),
        };
        self.dispatch.dispatch(req).await.map_err(|e| js_error_to_seam(&e))
    }

    /// Decode a reply's rows into neutral [`SeamRow`]s.
    fn rows_of(reply: &JsReply) -> Result<Vec<SeamRow>, SeamError> {
        reply
            .rows
            .iter()
            .map(row_to_seam)
            .collect::<Result<Vec<_>, _>>()
            .map_err(SeamError::message)
    }
}

impl<D: VerbDispatch> PgSession for NapiHostSession<D> {
    async fn batch_execute(&self, sql: &str) -> Result<(), SeamError> {
        let _g = self.enter();
        let req = JsRequest {
            kind: KIND_BATCH.to_string(),
            sql: sql.to_string(),
            binds: Vec::new(),
            text_params: Vec::new(),
        };
        match self.dispatch.dispatch(req).await {
            Ok(_) => Ok(()),
            Err(e) => Err(js_error_to_seam(&e)),
        }
    }

    async fn execute(&self, sql: &str, params: &[SeamBind]) -> Result<u64, SeamError> {
        let reply = self.call(KIND_EXECUTE, sql, params).await?;
        Ok(affected(&reply))
    }

    async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, SeamError> {
        let _g = self.enter();
        // Text-format params: cross verbatim as `(string | null)[]` (§B.2) — NO
        // type coercion, NO explicit OID. `None → null → PG NULL`.
        let req = JsRequest {
            kind: KIND_EXECUTE_TEXT.to_string(),
            sql: sql.to_string(),
            binds: Vec::new(),
            text_params: params.to_vec(),
        };
        let reply = self.dispatch.dispatch(req).await.map_err(|e| js_error_to_seam(&e))?;
        Ok(affected(&reply))
    }

    async fn query(&self, sql: &str, params: &[SeamBind]) -> Result<Vec<SeamRow>, SeamError> {
        let reply = self.call(KIND_QUERY, sql, params).await?;
        Self::rows_of(&reply)
    }

    async fn query_one(&self, sql: &str, params: &[SeamBind]) -> Result<SeamRow, SeamError> {
        let reply = self.call(KIND_QUERY_ONE, sql, params).await?;
        Self::rows_of(&reply)?
            .into_iter()
            .next()
            .ok_or_else(|| SeamError::message("query_one: host returned no rows"))
    }
}

/// Affected-row count for an `execute`/`executeTextParams` reply: the driver's
/// `result.rowCount` (node-pg) when present, else the returned rowset length. The
/// engine's `execute` consumers branch on `== 0` vs `> 0` (journal write-back /
/// idempotent recovery), so a faithful count matters.
fn affected(reply: &JsReply) -> u64 {
    match reply.row_count {
        Some(c) if c >= 0.0 => c as u64,
        _ => reply.rows.len() as u64,
    }
}

/// The one-in-flight guard (§B.6) — the exact discipline the MySQL `JsDriverBackend`
/// uses (`transport.rs` `in_flight`), lifted to `AtomicBool` because the seam is
/// `&self`.
struct InFlightGuard<'a>(&'a AtomicBool);

impl<'a> InFlightGuard<'a> {
    fn enter(flag: &'a AtomicBool) -> Self {
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            panic!(
                "PgSession verb issued while another is in flight — the host bridge \
                 pins ONE connection and the engine must be strictly one-verb-at-a-time (§B.6)"
            );
        }
        Self(flag)
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
