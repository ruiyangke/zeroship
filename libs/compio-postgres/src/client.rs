// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The user-facing query/prepare/execute/simple_query methods dispatch to
// the real `query.rs`/`prepare.rs`/`simple_query.rs` modules. Transaction
// and COPY helpers are still stubbed until `transaction.rs`,
// `copy_in.rs`, and `copy_out.rs` land.

use crate::cancel_token::CancelKey;
use crate::codec::{BackendMessages, FrontendMessage};
use crate::config::{ProtocolVersion, SslMode, SslNegotiation};
use crate::connect_tls::Encryption;
use crate::connection::{Request, RequestDisposition, RequestMessages, TransactionEffect};
use crate::copy_in::CopyInSink;
use crate::copy_out::CopyOutStream;
use crate::keepalive::KeepaliveConfig;
use crate::query::RowStream;
use crate::release::ConnectionRelease;
use crate::simple_query::SimpleQueryStream;
use crate::tls::{MakeTlsConnect, ServerVerification, TlsConnect};
use crate::types::{Oid, ToSql, Type};
use crate::{
    CancelToken, Error, Row, SimpleQueryMessage, Socket, Statement, ToStatement, Transaction,
    TransactionBuilder, copy_in, copy_out, prepare, query, simple_query, slice_iter,
};
use bytes::{Buf, Bytes, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use futures_util::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use postgres_types::{BorrowToSql, FromSqlOwned};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future;
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

/// The result of one SQL execution reported by the query observer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryOutcome {
    /// PostgreSQL completed the execution successfully.
    Success,
    /// PostgreSQL rejected the execution.
    ///
    /// Only SQLSTATE is retained. Server error text and detail can echo bound
    /// values, so they are deliberately excluded from observation events.
    DatabaseError {
        /// The SQLSTATE carried by PostgreSQL, when its error frame was valid.
        code: Option<crate::error::SqlState>,
    },
    /// The caller dropped the response future or stream before its terminal
    /// protocol message, or PostgreSQL returned SQLSTATE `57014`.
    Cancelled,
}

/// A completed SQL execution.
///
/// The SQL text is reported verbatim, including placeholders such as `$1`.
/// Bound parameter values are never decoded, copied, or attached to the
/// event. SQL literals are part of the SQL text itself and are therefore
/// visible; callers should keep secrets in bound parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryEvent {
    sql: Arc<str>,
    elapsed: Duration,
    outcome: QueryOutcome,
    rows: Option<u64>,
}

impl QueryEvent {
    /// The exact SQL text supplied to PostgreSQL.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Time from enqueueing the execution until its terminal server response.
    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Whether the execution succeeded, failed, or was abandoned by its caller.
    #[must_use]
    pub const fn outcome(&self) -> &QueryOutcome {
        &self.outcome
    }

    /// Rows returned or affected when PostgreSQL supplied a meaningful count.
    #[must_use]
    pub const fn rows(&self) -> Option<u64> {
        self.rows
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryProtocol {
    Extended,
    Simple,
}

#[derive(Clone)]
struct QueryObserver(Arc<QueryObserverInner>);

struct QueryObserverInner {
    sender: mpsc::UnboundedSender<QueryEvent>,
    threshold: Option<Duration>,
    registry: Arc<Mutex<FrontendRegistry>>,
    active: AtomicBool,
}

#[derive(Default)]
struct FrontendRegistry {
    statements: HashMap<String, Arc<str>>,
    portals: HashMap<String, Arc<str>>,
}

#[derive(Clone)]
pub(crate) struct QueryObservation(Arc<QueryObservationInner>);

struct QueryObservationInner {
    observer: QueryObserver,
    started_at: Instant,
    state: Mutex<QueryObservationState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsumerDisposition {
    Pending,
    Completed,
    Cancelled,
}

struct QueryObservationState {
    sql: Option<Arc<str>>,
    protocol: Option<QueryProtocol>,
    enqueued: bool,
    consumer: ConsumerDisposition,
    data_rows: u64,
    command_rows: Option<u64>,
    portal_suspended: bool,
    error: Option<crate::error::SqlState>,
    server_completed_at: Option<Instant>,
    emitted: bool,
}

impl QueryObserver {
    /// Used by this module's own tests. `cargo build` reports it dead because
    /// it does not compile `cfg(test)`; `cargo test` does not.
    #[cfg(test)]
    fn new(sender: mpsc::UnboundedSender<QueryEvent>, threshold: Option<Duration>) -> Self {
        Self::with_registry(sender, threshold, Arc::default())
    }

    fn with_registry(
        sender: mpsc::UnboundedSender<QueryEvent>,
        threshold: Option<Duration>,
        registry: Arc<Mutex<FrontendRegistry>>,
    ) -> Self {
        Self(Arc::new(QueryObserverInner {
            sender,
            threshold,
            registry,
            active: AtomicBool::new(true),
        }))
    }

    fn is_active(&self) -> bool {
        self.0.active.load(Ordering::Relaxed) && !self.0.sender.is_closed()
    }

    fn inspect_frontend(&self, message: &FrontendMessage, observation: &QueryObservation) {
        let FrontendMessage::Raw(bytes) = message else {
            return;
        };
        let mut registry = self.0.registry.lock();
        inspect_frontend_frames(bytes, &mut registry, observation);
    }

    fn emit(&self, event: QueryEvent) {
        if self.0.sender.unbounded_send(event).is_err() {
            self.0.active.store(false, Ordering::Relaxed);
        }
    }

    fn reports_elapsed(&self, elapsed: Duration) -> bool {
        query_elapsed_meets_threshold(elapsed, self.0.threshold)
    }

    fn filters_by_elapsed(&self) -> bool {
        self.0
            .threshold
            .is_some_and(|threshold| !threshold.is_zero())
    }
}

impl QueryObservation {
    fn new(observer: QueryObserver) -> Self {
        Self(Arc::new(QueryObservationInner {
            observer,
            started_at: Instant::now(),
            state: Mutex::new(QueryObservationState {
                sql: None,
                protocol: None,
                enqueued: false,
                consumer: ConsumerDisposition::Pending,
                data_rows: 0,
                command_rows: None,
                portal_suspended: false,
                error: None,
                server_completed_at: None,
                emitted: false,
            }),
        }))
    }

    fn set_execution(&self, sql: Arc<str>, protocol: QueryProtocol) {
        let mut state = self.0.state.lock();
        if state.sql.is_none() {
            state.sql = Some(sql);
            state.protocol = Some(protocol);
        }
        drop(state);
        self.maybe_emit();
    }

    fn mark_enqueued(&self) {
        self.0.state.lock().enqueued = true;
        self.maybe_emit();
    }

    pub(crate) fn is_execution(&self) -> bool {
        self.0.state.lock().sql.is_some()
    }

    pub(crate) fn filters_by_elapsed(&self) -> bool {
        self.0.observer.filters_by_elapsed()
    }

    pub(crate) fn inspect_frontend(&self, message: &FrontendMessage) {
        self.0.observer.inspect_frontend(message, self);
    }

    pub(crate) fn observe_server_message(&self, message: &Message) {
        let mut state = self.0.state.lock();
        match message {
            Message::DataRow(_) => state.data_rows = state.data_rows.saturating_add(1),
            Message::CommandComplete(body) => {
                state.command_rows = query::extract_row_affected(body).ok();
            }
            Message::PortalSuspended => state.portal_suspended = true,
            Message::ErrorResponse(body) => state.error = error_sqlstate(body),
            _ => {}
        }
    }

    pub(crate) fn server_complete(&self, at: Instant) {
        self.0.state.lock().server_completed_at.get_or_insert(at);
        self.maybe_emit();
    }

    pub(crate) fn filter_completed_at(&self, at: Instant) -> bool {
        let elapsed = at.saturating_duration_since(self.0.started_at);
        if self.0.observer.reports_elapsed(elapsed) {
            return false;
        }

        let mut state = self.0.state.lock();
        state.server_completed_at.get_or_insert(at);
        state.emitted = true;
        true
    }

    pub(crate) fn connection_closed(&self) {
        let mut state = self.0.state.lock();
        if state.server_completed_at.is_none() {
            state.server_completed_at = Some(Instant::now());
            state.error = Some(crate::error::SqlState::QUERY_CANCELED);
        }
        drop(state);
        self.maybe_emit();
    }

    fn observe_consumer_message(&self, message: &Message) {
        let mut state = self.0.state.lock();
        if state.consumer != ConsumerDisposition::Pending {
            return;
        }
        let extended_terminal = state.protocol == Some(QueryProtocol::Extended)
            && matches!(
                message,
                Message::CommandComplete(_)
                    | Message::PortalSuspended
                    | Message::CopyDone
                    | Message::EmptyQueryResponse
            );
        if extended_terminal
            || matches!(
                message,
                Message::ErrorResponse(_) | Message::ReadyForQuery(_)
            )
        {
            state.consumer = ConsumerDisposition::Completed;
            drop(state);
            self.maybe_emit();
        }
    }

    fn cancel_consumer(&self) {
        let mut state = self.0.state.lock();
        if state.consumer == ConsumerDisposition::Pending {
            state.consumer = ConsumerDisposition::Cancelled;
            drop(state);
            self.maybe_emit();
        }
    }

    fn maybe_emit(&self) {
        let event = {
            let mut state = self.0.state.lock();
            if state.emitted
                || !state.enqueued
                || state.consumer == ConsumerDisposition::Pending
                || state.server_completed_at.is_none()
                || state.sql.is_none()
            {
                return;
            }

            let completed_at = state
                .server_completed_at
                .expect("checked that server completion is present");
            let elapsed = completed_at.saturating_duration_since(self.0.started_at);
            state.emitted = true;
            if !self.0.observer.reports_elapsed(elapsed) {
                return;
            }

            let outcome = if state.consumer == ConsumerDisposition::Cancelled
                || state.error.as_ref() == Some(&crate::error::SqlState::QUERY_CANCELED)
            {
                QueryOutcome::Cancelled
            } else if state.error.is_some() {
                QueryOutcome::DatabaseError {
                    code: state.error.clone(),
                }
            } else {
                QueryOutcome::Success
            };
            let rows = match outcome {
                QueryOutcome::Success => state.command_rows.or_else(|| {
                    (state.data_rows != 0 || state.portal_suspended).then_some(state.data_rows)
                }),
                _ => None,
            };
            QueryEvent {
                sql: state.sql.clone().expect("checked that SQL is present"),
                elapsed,
                outcome,
                rows,
            }
        };
        self.0.observer.emit(event);
    }
}

fn query_elapsed_meets_threshold(elapsed: Duration, threshold: Option<Duration>) -> bool {
    threshold.is_none_or(|threshold| elapsed >= threshold)
}

fn error_sqlstate(
    body: &postgres_protocol::message::backend::ErrorResponseBody,
) -> Option<crate::error::SqlState> {
    let mut fields = body.fields();
    while let Some(field) = fields.next().ok()? {
        if field.type_() == b'C' {
            let code = std::str::from_utf8(field.value_bytes()).ok()?;
            return Some(crate::error::SqlState::from_code(code));
        }
    }
    None
}

fn cstr(input: &[u8]) -> Option<(&str, &[u8])> {
    let end = input.iter().position(|byte| *byte == 0)?;
    let value = std::str::from_utf8(&input[..end]).ok()?;
    Some((value, &input[end + 1..]))
}

fn inspect_frontend_frames(
    mut bytes: &[u8],
    registry: &mut FrontendRegistry,
    observation: &QueryObservation,
) {
    while bytes.len() >= 5 {
        let length = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        let Some(frame_len) = length.checked_add(1) else {
            return;
        };
        if length < 4 || frame_len > bytes.len() {
            return;
        }
        let tag = bytes[0];
        let body = &bytes[5..frame_len];
        match tag {
            b'P' => {
                if let Some((name, rest)) = cstr(body)
                    && let Some((sql, _)) = cstr(rest)
                {
                    registry
                        .statements
                        .insert(name.to_string(), Arc::<str>::from(sql));
                }
            }
            b'B' => {
                if let Some((portal, rest)) = cstr(body)
                    && let Some((statement, _)) = cstr(rest)
                {
                    if let Some(sql) = registry.statements.get(statement).cloned() {
                        registry.portals.insert(portal.to_string(), sql);
                    } else {
                        registry.portals.remove(portal);
                    }
                }
            }
            b'E' => {
                if let Some((portal, _)) = cstr(body)
                    && let Some(sql) = registry.portals.get(portal).cloned()
                {
                    observation.set_execution(sql, QueryProtocol::Extended);
                }
            }
            b'Q' => {
                if let Some((sql, _)) = cstr(body) {
                    observation.set_execution(Arc::<str>::from(sql), QueryProtocol::Simple);
                }
            }
            b'C' if !body.is_empty() => {
                if let Some((name, _)) = cstr(&body[1..]) {
                    match body[0] {
                        b'S' => {
                            registry.statements.remove(name);
                        }
                        b'P' => {
                            registry.portals.remove(name);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        bytes = &bytes[frame_len..];
    }
}

#[cfg(test)]
mod query_observer_threshold_tests {
    use super::{QueryObserver, query_elapsed_meets_threshold};
    use futures_channel::mpsc;
    use std::time::Duration;

    #[test]
    fn threshold_includes_the_exact_boundary() {
        let threshold = Duration::from_millis(50);

        assert!(!query_elapsed_meets_threshold(
            threshold - Duration::from_nanos(1),
            Some(threshold),
        ));
        assert!(query_elapsed_meets_threshold(threshold, Some(threshold)));
    }

    #[test]
    fn dropping_receiver_deactivates_observer_without_an_event() {
        let (sender, receiver) = mpsc::unbounded();
        let observer = QueryObserver::new(sender, Some(Duration::MAX));

        assert!(observer.is_active());
        drop(receiver);
        assert!(!observer.is_active());
    }
}

/// The transaction state the server reported in the most recent
/// `ReadyForQuery`.
///
/// Postgres appends this byte to every `ReadyForQuery`, so it is an exact,
/// already-paid-for answer to "is this session inside a transaction block?" -
/// no probe query required. The pool reads it on release to decide whether a
/// connection needs a rollback before the next borrower sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// Not inside a transaction block.
    Idle,
    /// Inside a transaction block.
    InTransaction,
    /// Inside a failed transaction block: the server rejects every statement
    /// with `25P02` until the block is rolled back.
    Failed,
}

impl TransactionStatus {
    /// Decode the `ReadyForQuery` status byte. An unknown byte reads as
    /// `Failed`, the conservative answer: it makes callers clear the session
    /// rather than assume it is reusable.
    const fn from_byte(byte: u8) -> Self {
        match byte {
            b'I' => Self::Idle,
            b'T' => Self::InTransaction,
            _ => Self::Failed,
        }
    }
}

#[cfg(test)]
mod transaction_status_tests {
    use super::{Client, TransactionStatus};
    use crate::codec::FrontendMessage;
    use crate::config::{ProtocolVersion, SslMode, SslNegotiation};
    use crate::connection::RequestMessages;
    use bytes::BytesMut;
    use futures_channel::mpsc;

    #[test]
    fn unknown_ready_for_query_status_is_conservatively_failed() {
        assert_eq!(
            TransactionStatus::from_byte(b'X'),
            TransactionStatus::Failed
        );
    }

    #[test]
    fn failed_send_does_not_leave_transaction_status_unsettled() {
        let (sender, receiver) = mpsc::unbounded();
        drop(receiver);

        let client = Client::new(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));

        let result = client
            .inner()
            .send(RequestMessages::Single(FrontendMessage::Raw(
                BytesMut::new().freeze(),
            )));
        assert!(
            result.is_err(),
            "the disconnected request channel accepted a send"
        );

        assert!(
            !client.has_in_flight_requests(),
            "failed send leaked its transaction-capable in-flight count"
        );
        assert_eq!(
            client.transaction_status(),
            Some(TransactionStatus::Idle),
            "failed send left the settled session looking unsettled"
        );
    }
}

/// A stream of backend messages for a single in-flight request.
///
/// Yields messages one at a time via `next().await`. The connection task
/// sends one or MORE `BackendMessages` batches per `RowDescription +
/// DataRow* + CommandComplete + ReadyForQuery` group, and `Responses`
/// flattens the inner `FallibleIterator` so the caller sees a linear message
/// stream. Matches tokio-postgres's `Responses` type.
///
/// One batch per group would be wrong to rely on: the decoder cuts a batch at
/// the last COMPLETE message in the read buffer, so a response larger than the
/// socket hands over at once arrives as several batches, only the last of
/// which carries `ReadyForQuery`. Bounding the read buffer requires that; see
/// `codec::read_backend`.
pub struct Responses {
    receiver: mpsc::Receiver<ResponseMessages>,
    cur: ResponseMessages,
    observation: Option<QueryObservation>,
}

pub(crate) enum ResponseMessages {
    Raw(BackendMessages),
    Observed(VecDeque<Result<Message, Error>>),
    /// The terminal response completed below its minimum duration, so retaining
    /// observer state while the caller decodes it would produce no event.
    Filtered(BackendMessages),
}

impl ResponseMessages {
    fn empty() -> Self {
        Self::Raw(BackendMessages::empty())
    }

    fn next(&mut self) -> Result<Option<Message>, Error> {
        match self {
            Self::Raw(messages) | Self::Filtered(messages) => messages.next().map_err(Error::parse),
            Self::Observed(messages) => messages.pop_front().transpose(),
        }
    }

    fn ends_observation(&self) -> bool {
        matches!(self, Self::Filtered(_))
    }
}

impl Responses {
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Result<Message, Error>> {
        loop {
            match self.cur.next()? {
                Some(message) => {
                    if let Some(observation) = &self.observation {
                        observation.observe_consumer_message(&message);
                    }
                    if let Message::ErrorResponse(body) = message {
                        return Poll::Ready(Err(Error::db(body)));
                    }
                    return Poll::Ready(Ok(message));
                }
                None => {}
            }

            match ready!(self.receiver.poll_next_unpin(cx)) {
                Some(messages) => {
                    if messages.ends_observation() {
                        self.observation = None;
                    }
                    self.cur = messages;
                }
                None => return Poll::Ready(Err(Error::closed())),
            }
        }
    }

    /// Pull the next backend message from the response stream.
    pub async fn next(&mut self) -> Result<Message, Error> {
        future::poll_fn(|cx| self.poll_next(cx)).await
    }
}

impl Drop for Responses {
    fn drop(&mut self) {
        if let Some(observation) = &self.observation {
            observation.cancel_consumer();
        }
    }
}

/// A cache of type info and prepared statements for fetching type info.
#[derive(Default)]
pub(crate) struct CachedTypeInfo {
    /// A statement for basic information for a type from its OID.
    pub(crate) typeinfo: Option<Statement>,
    /// A statement for getting information for a composite type from its OID.
    pub(crate) typeinfo_composite: Option<Statement>,
    /// A statement for getting information for an enum type from its OID.
    pub(crate) typeinfo_enum: Option<Statement>,
    /// Cache of types already looked up.
    pub(crate) types: HashMap<Oid, Type>,
    /// Separates lookups which began before a clear from the cache state that
    /// the clear established.
    generation: u64,
}

/// Exact-SQL LRU for statements prepared implicitly by raw-string operations.
/// The capacity lives here, rather than on `Client`, so every physical
/// `PostgreSQL` connection owns an independent cache.
#[derive(Default)]
struct StatementCache {
    statements: HashMap<Arc<str>, Statement>,
    lru: VecDeque<Arc<str>>,
    candidates: HashMap<Arc<str>, usize>,
    candidate_lru: VecDeque<Arc<str>>,
}

/// Keep admission bookkeeping independent of the configured prepared slots.
/// One hundred mirrors Npgsql's bounded auto-prepare candidate pool: it is
/// broad enough for a normal working set while making the worst-case number of
/// extra retained SQL strings predictable regardless of user cache capacity.
const STATEMENT_CACHE_CANDIDATE_CAPACITY: usize = 100;

pub(crate) enum StatementCacheAdmission {
    Cached(Statement),
    PrepareNamed,
    ExecuteUnnamed,
}

fn statement_uses_cached_typeinfo(statement: &Statement) -> bool {
    statement
        .params()
        .iter()
        .chain(statement.columns().iter().map(|column| column.type_()))
        // Built-in OIDs bypass CachedTypeInfo, so only a custom descriptor can
        // retain one of the immutable Type snapshots being invalidated.
        .any(|type_| Type::from_oid(type_.oid()).is_none())
}

/// Whether reparsing can heal this cached-statement failure.
///
/// Match `26000` broadly, as pgjdbc does. `0A000` covers many unrelated
/// unsupported features, so match PostgreSQL's plan-cache routines rather
/// than hiding every error in that broad SQLSTATE class. These are the same
/// stable routine names pgjdbc uses for its one-shot reparse decision.
fn cached_statement_error_can_retry(error: &Error) -> bool {
    match error.code() {
        Some(code) if code == &crate::error::SqlState::INVALID_SQL_STATEMENT_NAME => true,
        Some(code) if code == &crate::error::SqlState::FEATURE_NOT_SUPPORTED => error
            .as_db_error()
            .and_then(crate::error::DbError::routine)
            .is_some_and(|routine| {
                matches!(routine, "RevalidateCachedQuery" | "RevalidateCachedPlan")
            }),
        _ => false,
    }
}

#[cfg(test)]
mod type_cache_tests {
    use super::{
        Client, STATEMENT_CACHE_CANDIDATE_CAPACITY, StatementCacheAdmission,
        StatementCacheSettings, statement_uses_cached_typeinfo,
    };
    use crate::config::{ProtocolVersion, SslMode, SslNegotiation};
    use crate::types::{Kind, Type};
    use crate::{Error, Statement};
    use bytes::{BufMut, BytesMut};
    use futures_channel::mpsc;
    use postgres_protocol::message::backend::Message;
    use std::num::NonZeroUsize;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    fn custom_type() -> Type {
        Type::new(
            "cache_enum".to_string(),
            900_001,
            Kind::Enum(vec!["before".to_string()]),
            "test".to_string(),
        )
    }

    fn client_with_statement_cache() -> Client {
        let (sender, _receiver) = mpsc::unbounded();
        Client::new_with_statement_cache(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(1, NonZeroUsize::MIN),
        )
    }

    fn invalid_statement_name_error() -> Error {
        let payload = b"SERROR\0C26000\0Mscripted stale statement\0\0";
        let mut frame = BytesMut::new();
        frame.put_u8(b'E');
        frame.put_u32(u32::try_from(payload.len() + 4).unwrap());
        frame.extend_from_slice(payload);

        match Message::parse(&mut frame).expect("parse scripted ErrorResponse") {
            Some(Message::ErrorResponse(body)) => Error::db(body),
            _ => panic!("scripted 26000 did not decode as ErrorResponse"),
        }
    }

    #[test]
    fn encoder_panic_does_not_leak_bytes_into_the_next_message() {
        let client = client_with_statement_cache();

        let panic = catch_unwind(AssertUnwindSafe(|| {
            client.inner().with_buf(|buf| {
                buf.extend_from_slice(b"stale frontend bytes");
                panic!("scripted encoder panic");
            });
        }));
        assert!(panic.is_err(), "the scripted encoder did not panic");

        client.inner().with_buf(|buf| {
            assert!(
                buf.is_empty(),
                "a panicking encoder left stale bytes for the next message: {buf:?}"
            );
        });
    }

    #[test]
    fn late_cached_error_does_not_evict_newer_same_sql_statement() {
        const SQL: &str = "SELECT 1";

        let client = client_with_statement_cache();
        let generation = client.inner().type_cache_generation();
        let old = Statement::new(
            client.inner(),
            "old".to_string(),
            Vec::new(),
            Vec::new(),
            false,
        );
        let old_winner = client.inner().cache_statement(SQL, old.clone(), generation);
        assert!(old_winner.same_instance(&old));

        let replacement = Statement::new(
            client.inner(),
            "replacement".to_string(),
            Vec::new(),
            Vec::new(),
            false,
        );
        let displaced = {
            let mut cache = client.inner().statement_cache.lock();
            cache
                .statements
                .insert(Arc::<str>::from(SQL), replacement.clone())
                .expect("old statement was cached")
        };
        drop(displaced);

        client
            .inner()
            .invalidate_cached_statement_on_error(&old, &invalid_statement_name_error());

        let survivor = client
            .inner()
            .cached_statement(SQL)
            .expect("a late error evicted the newer same-SQL statement");
        assert!(
            survivor.same_instance(&replacement),
            "a late error replaced the newer same-SQL cache identity"
        );
    }

    #[test]
    fn custom_parameter_marks_an_implicit_statement_for_eviction() {
        let statement = Statement::unnamed(vec![custom_type()], Vec::new());

        assert!(statement_uses_cached_typeinfo(&statement));
    }

    #[test]
    fn builtin_descriptors_do_not_mark_an_implicit_statement_for_eviction() {
        let statement = Statement::unnamed(vec![Type::INT4, Type::TEXT], Vec::new());

        assert!(!statement_uses_cached_typeinfo(&statement));
    }

    #[test]
    fn clear_rejects_type_and_statement_insertions_from_its_old_generation() {
        let client = client_with_statement_cache();
        let generation = client.inner().type_cache_generation();
        let custom = custom_type();
        let statement = Statement::new(
            client.inner(),
            "stale".to_string(),
            vec![custom.clone()],
            Vec::new(),
            false,
        );

        client.clear_type_cache();
        client.inner().set_type(custom.oid(), &custom, generation);
        let returned = client
            .inner()
            .cache_statement("SELECT $1", statement, generation);

        assert!(client.inner().cached_type(custom.oid()).0.is_none());
        assert!(client.inner().cached_statement("SELECT $1").is_none());
        drop(returned);
    }

    #[test]
    fn statement_cache_admission_candidates_have_a_hard_bound() {
        let (sender, _receiver) = mpsc::unbounded();
        let client = Client::new_with_statement_cache(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(1, NonZeroUsize::new(usize::MAX).unwrap()),
        );

        for index in 0..=STATEMENT_CACHE_CANDIDATE_CAPACITY {
            assert!(matches!(
                client
                    .inner()
                    .statement_cache_admission(&format!("SELECT {index}")),
                StatementCacheAdmission::ExecuteUnnamed
            ));
        }

        let cache = client.inner().statement_cache.lock();
        assert_eq!(cache.candidates.len(), STATEMENT_CACHE_CANDIDATE_CAPACITY);
        assert_eq!(
            cache.candidate_lru.len(),
            STATEMENT_CACHE_CANDIDATE_CAPACITY
        );
        assert!(!cache.candidates.contains_key("SELECT 0"));
    }

    #[test]
    fn statement_cache_admission_forgets_a_stale_near_threshold_working_set() {
        let (sender, _receiver) = mpsc::unbounded();
        let client = Client::new_with_statement_cache(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(1, NonZeroUsize::new(3).unwrap()),
        );

        for index in 0..STATEMENT_CACHE_CANDIDATE_CAPACITY {
            let sql = format!("SELECT stale_{index}");
            assert!(matches!(
                client.inner().statement_cache_admission(&sql),
                StatementCacheAdmission::ExecuteUnnamed
            ));
            assert!(matches!(
                client.inner().statement_cache_admission(&sql),
                StatementCacheAdmission::ExecuteUnnamed
            ));
        }

        for sql in ["SELECT fresh_a", "SELECT fresh_b"] {
            assert!(matches!(
                client.inner().statement_cache_admission(sql),
                StatementCacheAdmission::ExecuteUnnamed
            ));
        }
        for sql in ["SELECT fresh_a", "SELECT fresh_b"] {
            assert!(matches!(
                client.inner().statement_cache_admission(sql),
                StatementCacheAdmission::ExecuteUnnamed
            ));
        }
        assert!(matches!(
            client.inner().statement_cache_admission("SELECT fresh_a"),
            StatementCacheAdmission::PrepareNamed
        ));
    }
}

/// Shared inner state of a `Client`. Lives behind an `Arc` so helpers
/// like `bind`, `prepare`, and the Drop impls on `Statement`/`Portal`
/// can keep a `Weak<InnerClient>` back-reference.
pub struct InnerClient {
    sender: mpsc::UnboundedSender<Request>,
    query_observer_enabled: AtomicBool,
    query_observer: Mutex<Option<QueryObserver>>,
    cached_typeinfo: Mutex<CachedTypeInfo>,
    statement_cache_capacity: usize,
    statement_cache_execution_threshold: NonZeroUsize,
    statement_cache: Mutex<StatementCache>,

    /// Scratch buffer for encoding frontend messages. `with_buf` locks
    /// this, hands the caller a `&mut BytesMut`, and clears on drop so
    /// the next caller sees a fresh buffer.
    buffer: Mutex<BytesMut>,

    /// Set when a fire-and-forget ROLLBACK has been queued (e.g. by
    /// `Transaction::drop`) but not yet observed to completion. The pool
    /// uses this at checkout time to run a barrier (`simple_query("")`)
    /// that drains the pending ROLLBACK before the connection is handed
    /// to the next caller. Cleared once the pool confirms the connection
    /// is clean, or on explicit commit/rollback (which await completion
    /// synchronously).
    ///
    /// `AtomicBool` with `Relaxed` ordering: the pool and the Client are
    /// on the same thread in the current usage model, but `Client` is
    /// nominally `Send`, so an atomic keeps the bound clean without
    /// invoking UB on a future cross-thread move.
    dirty: AtomicBool,

    /// Transaction-status byte from the last `ReadyForQuery` the connection
    /// task received (`I`/`T`/`E`), starting at `I` because a session that has
    /// just finished startup is idle. The connection task is the sole writer;
    /// the pool reads it on release.
    tx_status: Arc<AtomicU8>,

    /// Transaction-capable requests whose `ReadyForQuery` has not reached the
    /// connection task yet. This is separate from `tx_status`: a pooled client
    /// can be released before the task gets a chance to observe the response,
    /// so an idle status is authoritative only when this count is zero. The
    /// driver's internal `Close + Sync` maintenance is not counted because it
    /// cannot change transaction state.
    in_flight_requests: Arc<AtomicUsize>,

    /// Shuts the connection's socket down when this `InnerClient` drops - that
    /// is, when the last handle that could still issue a query on this
    /// connection goes away.
    ///
    /// Dropping the client also ASKS the connection task to terminate when the
    /// `sender` above closes, but that request is only honoured if something
    /// polls the task afterwards, and nothing guarantees that: a compio runtime
    /// torn down with the task parked leaves the socket - and with it the
    /// server-side backend - alive for the rest of the process. `InnerClient`'s
    /// `Drop` takes this field first so TLS release cannot race the sender's
    /// asynchronous teardown. See [`crate::release`] for the mechanism and the
    /// measurement.
    ///
    /// `None` for [`Config::connect_raw`](crate::Config::connect_raw), whose
    /// stream belongs to the caller and need not be a socket at all.
    release: Option<ConnectionRelease>,

    /// Server runtime parameters as last reported by `ParameterStatus`, shared
    /// with the connection task.
    ///
    /// Seeded from the startup sequence and then maintained by the connection
    /// task, which is the SOLE WRITER: it observes every frame exactly once and
    /// in wire order, so a value here is the most recent one the server sent.
    /// Readers take the lock only in [`Client::parameter`]; nothing on a query
    /// hot path touches it, and `route_async` locks it only for the
    /// `ParameterStatus` arm.
    parameters: Arc<Mutex<HashMap<String, String>>>,
}

impl Drop for InnerClient {
    fn drop(&mut self) {
        // Struct fields normally drop in declaration order, which would close
        // `sender` first and let the connection task race this synchronous TLS
        // release. Take it while every request-channel handle is still alive.
        drop(self.release.take());
    }
}

struct ClearBufferOnDrop<'a>(&'a mut BytesMut);

impl Drop for ClearBufferOnDrop<'_> {
    fn drop(&mut self) {
        self.0.clear();
    }
}

impl InnerClient {
    /// Send a batch of frontend messages to the connection task. Returns
    /// a `Responses` stream the caller drains with `next().await`.
    ///
    /// The connection task owns the socket; this method only enqueues
    /// into the unbounded `UnboundedSender<Request>` channel. Failure
    /// means the connection task has terminated (socket closed).
    pub fn send(&self, messages: RequestMessages) -> Result<Responses, Error> {
        self.send_inner(
            messages,
            RequestDisposition::Awaited,
            TransactionEffect::MayChange,
            None,
            None,
        )
    }

    /// Retain a potentially cached statement with the connection-side response
    /// state. That task outlives a cancelled caller and is therefore the only
    /// place guaranteed to see an error which must invalidate the cache.
    pub(crate) fn send_statement(
        &self,
        messages: RequestMessages,
        statement: &Statement,
    ) -> Result<Responses, Error> {
        let statement = Some(statement.clone());
        self.send_inner(
            messages,
            RequestDisposition::Awaited,
            TransactionEffect::MayChange,
            None,
            statement,
        )
    }

    /// Send a Parse request with cancellation ownership tracked by the
    /// connection task until it observes ParseComplete or ErrorResponse.
    pub(crate) fn send_prepare(
        &self,
        messages: RequestMessages,
        cleanup: prepare::PrepareCleanup,
    ) -> Result<Responses, Error> {
        self.send_inner(
            messages,
            RequestDisposition::Awaited,
            TransactionEffect::MayChange,
            Some(cleanup),
            None,
        )
    }

    /// Send a request whose response obligation and transaction effect differ
    /// from the common awaited, transaction-capable case.
    pub(crate) fn send_with(
        &self,
        messages: RequestMessages,
        disposition: RequestDisposition,
        transaction_effect: TransactionEffect,
    ) -> Result<Responses, Error> {
        self.send_inner(messages, disposition, transaction_effect, None, None)
    }

    /// Enqueue a known-failing request and its recovery without allowing a
    /// later caller request to overtake the recovery.
    pub(crate) fn send_with_error_cleanup(
        &self,
        message: FrontendMessage,
        cleanup: FrontendMessage,
    ) -> Result<Responses, Error> {
        let responses = self.send(RequestMessages::Single(message))?;
        drop(self.send_with(
            RequestMessages::Single(cleanup),
            RequestDisposition::Housekeeping,
            TransactionEffect::MayChange,
        )?);
        Ok(responses)
    }

    fn send_inner(
        &self,
        messages: RequestMessages,
        disposition: RequestDisposition,
        transaction_effect: TransactionEffect,
        prepare_cleanup: Option<prepare::PrepareCleanup>,
        statement: Option<Statement>,
    ) -> Result<Responses, Error> {
        let observation = self.start_observation(&messages);
        let (sender, receiver) = mpsc::channel(1);
        let request = Request {
            messages,
            sender,
            disposition,
            transaction_effect,
            prepare_cleanup,
            statement,
            observation: observation.clone(),
        };
        if transaction_effect == TransactionEffect::MayChange {
            self.in_flight_requests.fetch_add(1, Ordering::Relaxed);
        }
        if self.sender.unbounded_send(request).is_err() {
            if transaction_effect == TransactionEffect::MayChange {
                self.in_flight_requests.fetch_sub(1, Ordering::Relaxed);
            }
            return Err(Error::closed());
        }

        if let Some(observation) = &observation {
            observation.mark_enqueued();
        }

        Ok(Responses {
            receiver,
            cur: ResponseMessages::empty(),
            observation,
        })
    }

    fn start_observation(&self, messages: &RequestMessages) -> Option<QueryObservation> {
        if !self.query_observer_enabled.load(Ordering::Acquire) {
            return None;
        }

        let observer = {
            let observer = self.query_observer.lock();
            let selected = if observer.as_ref().is_some_and(QueryObserver::is_active) {
                observer.clone()
            } else {
                self.query_observer_enabled.store(false, Ordering::Release);
                None
            };
            drop(observer);
            selected
        }?;

        let observation = QueryObservation::new(observer.clone());
        if let RequestMessages::Single(message) = messages {
            observer.inspect_frontend(message, &observation);
        }
        Some(observation)
    }

    fn install_query_observer(
        &self,
        sender: mpsc::UnboundedSender<QueryEvent>,
        threshold: Option<Duration>,
    ) {
        let mut observer = self.query_observer.lock();
        let registry = observer
            .as_ref()
            .map(|observer| Arc::clone(&observer.0.registry))
            .unwrap_or_default();
        *observer = Some(QueryObserver::with_registry(sender, threshold, registry));
        drop(observer);
        self.query_observer_enabled.store(true, Ordering::Release);
    }

    /// The transaction state the server reported in the last `ReadyForQuery`,
    /// or `None` while a transaction-capable request has yet to reach its own.
    ///
    /// The in-flight check comes FIRST, and its `Acquire` is what publishes the
    /// task's `Relaxed` status store, so a `Some` is never stale.
    pub(crate) fn transaction_status(&self) -> Option<TransactionStatus> {
        if self.has_in_flight_requests() {
            return None;
        }
        Some(TransactionStatus::from_byte(
            self.tx_status.load(Ordering::Relaxed),
        ))
    }

    /// Whether a transaction-capable request has not reached its terminating
    /// `ReadyForQuery` in the connection task. The acquire pairs with the
    /// task's release decrement, making a zero observation publish the
    /// preceding status store.
    pub(crate) fn has_in_flight_requests(&self) -> bool {
        self.in_flight_requests.load(Ordering::Acquire) != 0
    }

    pub(crate) fn typeinfo(&self) -> Option<Statement> {
        self.cached_typeinfo.lock().typeinfo.clone()
    }

    pub(crate) fn set_typeinfo(&self, statement: &Statement) {
        self.cached_typeinfo.lock().typeinfo = Some(statement.clone());
    }

    pub(crate) fn typeinfo_composite(&self) -> Option<Statement> {
        self.cached_typeinfo.lock().typeinfo_composite.clone()
    }

    pub(crate) fn set_typeinfo_composite(&self, statement: &Statement) {
        self.cached_typeinfo.lock().typeinfo_composite = Some(statement.clone());
    }

    pub(crate) fn typeinfo_enum(&self) -> Option<Statement> {
        self.cached_typeinfo.lock().typeinfo_enum.clone()
    }

    pub(crate) fn set_typeinfo_enum(&self, statement: &Statement) {
        self.cached_typeinfo.lock().typeinfo_enum = Some(statement.clone());
    }

    pub(crate) fn cached_type(&self, oid: Oid) -> (Option<Type>, u64) {
        let cache = self.cached_typeinfo.lock();
        (cache.types.get(&oid).cloned(), cache.generation)
    }

    pub(crate) fn set_type(&self, oid: Oid, type_: &Type, generation: u64) {
        let mut cache = self.cached_typeinfo.lock();
        if cache.generation == generation {
            cache.types.insert(oid, type_.clone());
        }
    }

    pub(crate) fn type_cache_generation(&self) -> u64 {
        self.cached_typeinfo.lock().generation
    }

    pub(crate) fn clear_type_cache(&self) {
        let removed = {
            let mut typeinfo = self.cached_typeinfo.lock();
            typeinfo.types.clear();
            typeinfo.generation = typeinfo.generation.wrapping_add(1);

            // Keeping the generation change and statement scan under one lock
            // order makes an in-flight prepare either precede this eviction or
            // observe the new generation and decline to populate the cache.
            let mut statements = self.statement_cache.lock();
            let keys = statements
                .statements
                .iter()
                .filter(|(_, statement)| statement_uses_cached_typeinfo(statement))
                .map(|(key, _)| Arc::clone(key))
                .collect::<Vec<_>>();

            statements.lru.retain(|key| !keys.contains(key));
            let removed = keys
                .into_iter()
                .filter_map(|key| statements.statements.remove(&key))
                .collect::<Vec<_>>();
            drop(statements);
            drop(typeinfo);
            removed
        };

        // Statement teardown takes the encoding buffer and enqueues protocol
        // work, neither of which belongs inside the cache's critical section.
        drop(removed);
    }

    pub(crate) const fn statement_cache_capacity(&self) -> usize {
        self.statement_cache_capacity
    }

    /// Choose the protocol path for this exact SQL execution.
    ///
    /// Candidate tracking is a bounded, separate admission cache. It evicts
    /// least-recently-used SQL so stale near-threshold entries cannot prevent a
    /// new repeating working set from accumulating enough executions.
    pub(crate) fn statement_cache_admission(&self, query: &str) -> StatementCacheAdmission {
        let mut cache = self.statement_cache.lock();
        if let Some((key, statement)) = cache
            .statements
            .get_key_value(query)
            .map(|(key, statement)| (Arc::clone(key), statement.clone()))
        {
            if let Some(position) = cache.lru.iter().position(|candidate| candidate == &key) {
                cache.lru.remove(position);
            }
            cache.lru.push_back(key);
            return StatementCacheAdmission::Cached(statement);
        }

        let threshold = self.statement_cache_execution_threshold.get();
        if threshold == 1 {
            return StatementCacheAdmission::PrepareNamed;
        }

        let (key, executions) = if let Some((key, executions)) = cache
            .candidates
            .get_key_value(query)
            .map(|(key, executions)| (Arc::clone(key), *executions))
        {
            let executions = executions.saturating_add(1).min(threshold);
            *cache
                .candidates
                .get_mut(query)
                .expect("the candidate was just found") = executions;
            (key, executions)
        } else {
            if cache.candidates.len() == STATEMENT_CACHE_CANDIDATE_CAPACITY {
                let evicted = cache
                    .candidate_lru
                    .pop_front()
                    .expect("every candidate has an LRU entry");
                cache.candidates.remove(&evicted);
            }

            let key: Arc<str> = Arc::from(query);
            cache.candidates.insert(Arc::clone(&key), 1);
            (key, 1)
        };

        if let Some(position) = cache
            .candidate_lru
            .iter()
            .position(|candidate| candidate == &key)
        {
            cache.candidate_lru.remove(position);
        }
        cache.candidate_lru.push_back(key);

        if executions == threshold {
            StatementCacheAdmission::PrepareNamed
        } else {
            StatementCacheAdmission::ExecuteUnnamed
        }
    }

    /// Look up an exact SQL string and promote it to most-recently used.
    pub(crate) fn cached_statement(&self, query: &str) -> Option<Statement> {
        let mut cache = self.statement_cache.lock();
        let (key, statement) = cache
            .statements
            .get_key_value(query)
            .map(|(key, statement)| (Arc::clone(key), statement.clone()))?;

        if let Some(position) = cache.lru.iter().position(|candidate| candidate == &key) {
            cache.lru.remove(position);
        }
        cache.lru.push_back(key);
        drop(cache);
        Some(statement)
    }

    /// Install a freshly prepared statement after a cold miss.
    ///
    /// Preparation deliberately happens without this lock held. A concurrent
    /// caller may therefore have installed a winner in the meantime; in that
    /// case this returns the winner and drops the losing statement only after
    /// releasing the cache lock. Its existing Drop implementation closes the
    /// losing server-side name.
    pub(crate) fn cache_statement(
        &self,
        query: &str,
        statement: Statement,
        type_cache_generation: u64,
    ) -> Statement {
        let mut candidate = Some(statement);
        let capacity = self.statement_cache_capacity();
        if capacity == 0 {
            return candidate.take().expect("the candidate is present");
        }

        let typeinfo = self.cached_typeinfo.lock();
        if typeinfo.generation != type_cache_generation {
            return candidate.take().expect("the candidate is present");
        }

        let (winner, evicted) = {
            let mut cache = self.statement_cache.lock();
            if let Some((key, _)) = cache
                .candidates
                .get_key_value(query)
                .map(|(key, executions)| (Arc::clone(key), *executions))
            {
                cache.candidates.remove(&key);
                cache.candidate_lru.retain(|candidate| candidate != &key);
            }
            let outcome = if let Some((key, winner)) = cache
                .statements
                .get_key_value(query)
                .map(|(key, statement)| (Arc::clone(key), statement.clone()))
            {
                if let Some(position) = cache.lru.iter().position(|candidate| candidate == &key) {
                    cache.lru.remove(position);
                }
                cache.lru.push_back(key);
                (winner, None)
            } else {
                let evicted = if cache.statements.len() == capacity {
                    cache
                        .lru
                        .pop_front()
                        .and_then(|key| cache.statements.remove(&key))
                } else {
                    None
                };

                let key: Arc<str> = Arc::from(query);
                let statement = candidate.take().expect("the candidate is present");
                let winner = statement.clone();
                cache.statements.insert(Arc::clone(&key), statement);
                cache.lru.push_back(key);
                (winner, evicted)
            };
            drop(cache);
            outcome
        };
        drop(typeinfo);

        // Both Drop paths can lock the encoding buffer and enqueue Close +
        // Sync, so neither is allowed to run while the cache mutex is held.
        drop(evicted);
        drop(candidate);
        winner
    }

    /// Remove only the cached entry backed by `statement` when `PostgreSQL` says
    /// that named statement is no longer usable. Identity matters: a late
    /// error from an evicted statement must not remove a newer replacement for
    /// the same SQL text.
    pub(crate) fn invalidate_cached_statement_on_error(
        &self,
        statement: &Statement,
        error: &Error,
    ) {
        let invalidates = error.code().is_some_and(|code| {
            code == &crate::error::SqlState::FEATURE_NOT_SUPPORTED
                || code == &crate::error::SqlState::INVALID_SQL_STATEMENT_NAME
        });
        if !invalidates {
            return;
        }

        // The type-info helpers are not implicit SQL-cache entries, but their
        // server-side names can be invalidated by the same events, and a stale
        // one is reused by the NEXT lookup rather than failing this one.
        let removed_typeinfo = {
            let mut cache = self.cached_typeinfo.lock();
            fn take_matching(
                slot: &mut Option<Statement>,
                statement: &Statement,
            ) -> Option<Statement> {
                if slot
                    .as_ref()
                    .is_some_and(|cached| cached.same_instance(statement))
                {
                    slot.take()
                } else {
                    None
                }
            }

            [
                take_matching(&mut cache.typeinfo, statement),
                take_matching(&mut cache.typeinfo_composite, statement),
                take_matching(&mut cache.typeinfo_enum, statement),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
        };

        let removed = {
            let mut cache = self.statement_cache.lock();
            let key = cache
                .statements
                .iter()
                .find_map(|(key, cached)| cached.same_instance(statement).then(|| Arc::clone(key)));

            let removed = key.and_then(|key| {
                cache.lru.retain(|candidate| candidate != &key);
                cache.statements.remove(&key)
            });
            drop(cache);
            removed
        };

        // Statement::drop enqueues Close + Sync and must not run under either
        // cache mutex.
        drop(removed);
        drop(removed_typeinfo);
    }

    /// Lock the shared scratch buffer, run `f`, and clear the buffer on
    /// exit. Used by encoders that want to build a frontend message
    /// without allocating a fresh `BytesMut` each time.
    pub fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut buffer = self.buffer.lock();
        let clear = ClearBufferOnDrop(&mut buffer);
        f(&mut *clear.0)
    }

    /// Mark the connection as "dirty" - a fire-and-forget message (e.g.
    /// ROLLBACK queued from `Transaction::drop`) is pending but not yet
    /// observed to completion.
    pub(crate) fn set_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Clear the dirty flag. Called after a confirmed-complete command
    /// (explicit `commit`/`rollback`) or after the pool's checkout barrier
    /// drains any pending fire-and-forget message.
    pub(crate) fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Relaxed);
    }

    /// Is a fire-and-forget message pending? If true, the pool must run a
    /// barrier (`simple_query("")`) before treating the connection as
    /// clean.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }
}

/// Captured by [`CancelToken`] so a cancel request can reach the same
/// backend the original connection did. `cancel_query.rs` reconnects
/// using these fields.
#[derive(Clone)]
pub(crate) struct SocketConfig {
    pub addr: Addr,
    pub hostname: Option<String>,
    pub port: u16,
    pub connect_timeout: Option<Duration>,
    pub tcp_user_timeout: Option<Duration>,
    pub keepalive: Option<KeepaliveConfig>,
    pub require_peer: Option<String>,
    /// The transport this session ACTUALLY negotiated - recorded, never
    /// re-derived.
    ///
    /// `sslmode` plus the address does not determine it. `allow` dials
    /// plaintext first and re-dials with TLS when the server refuses, so an
    /// `hostssl`-only server yields an encrypted session under a mode whose
    /// first offer is plaintext; `prefer` does the reverse. A cancel has no
    /// second leg, so guessing wrong either puts the cancel key on the wire in
    /// the clear or fails the cancel outright.
    pub encryption: Encryption,
    /// The server verification this session's `sslmode` and `sslrootcert`
    /// demanded - recorded here for the same reason as `encryption` above.
    ///
    /// A cancel carries the backend PID and secret key, which are a BEARER
    /// CREDENTIAL: anything holding them can cancel this session's queries. The
    /// connector used to send them is supplied by the CALLER at cancel time
    /// (`CancelToken::cancel_query` takes it as an argument) and need not be
    /// the one the session was vetted with, so the demand has to travel with
    /// the session rather than be taken on trust from whatever turns up later.
    pub server_verification: ServerVerification,
}

/// Resolved transport endpoint: either a concrete IP or a Unix socket
/// directory. Used by `connect_socket.rs` to pick the right stream type.
#[derive(Clone)]
pub(crate) enum Addr {
    Tcp(IpAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

/// An asynchronous PostgreSQL client handle.
///
/// The client is one half of what is returned when a connection is
/// established. Users interact with the database through this client;
/// the other half is the [`Connection`](crate::Connection) which drives
/// I/O on a background task.
pub struct Client {
    inner: Arc<InnerClient>,
    socket_config: Option<SocketConfig>,
    ssl_mode: SslMode,
    ssl_negotiation: SslNegotiation,
    process_id: i32,
    secret_key: Option<CancelKey>,
    /// What the startup exchange SETTLED ON, which is not necessarily what was
    /// requested: an older server answers `NegotiateProtocolVersion` and the
    /// session continues one version down.
    protocol_version: ProtocolVersion,
}

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Client>();
};

#[derive(Clone, Copy)]
pub(crate) struct StatementCacheSettings {
    capacity: usize,
    execution_threshold: NonZeroUsize,
}

impl StatementCacheSettings {
    pub(crate) const fn new(capacity: usize, execution_threshold: NonZeroUsize) -> Self {
        Self {
            capacity,
            execution_threshold,
        }
    }
}

impl Client {
    /// The cache-less constructor, used by this crate's own tests. `cargo
    /// build` reports it dead because it does not compile `cfg(test)`.
    #[cfg(test)]
    pub(crate) fn new(
        sender: mpsc::UnboundedSender<Request>,
        ssl_mode: SslMode,
        ssl_negotiation: SslNegotiation,
        process_id: i32,
        secret_key: Option<CancelKey>,
        release: Option<ConnectionRelease>,
    ) -> Self {
        Self::new_with_statement_cache(
            sender,
            ssl_mode,
            ssl_negotiation,
            process_id,
            secret_key,
            release,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(0, NonZeroUsize::MIN),
        )
    }

    pub(crate) fn new_with_statement_cache(
        sender: mpsc::UnboundedSender<Request>,
        ssl_mode: SslMode,
        ssl_negotiation: SslNegotiation,
        process_id: i32,
        secret_key: Option<CancelKey>,
        release: Option<ConnectionRelease>,
        protocol_version: ProtocolVersion,
        statement_cache: StatementCacheSettings,
    ) -> Self {
        Self {
            inner: Arc::new(InnerClient {
                sender,
                query_observer_enabled: AtomicBool::new(false),
                query_observer: Mutex::new(None),
                cached_typeinfo: Default::default(),
                statement_cache_capacity: statement_cache.capacity,
                statement_cache_execution_threshold: statement_cache.execution_threshold,
                statement_cache: Mutex::default(),
                buffer: Default::default(),
                dirty: AtomicBool::new(false),
                tx_status: Arc::new(AtomicU8::new(b'I')),
                in_flight_requests: Arc::new(AtomicUsize::new(0)),
                release,
                parameters: Arc::default(),
            }),
            socket_config: None,
            ssl_mode,
            ssl_negotiation,
            process_id,
            secret_key,
            protocol_version,
        }
    }

    pub(crate) fn inner(&self) -> &Arc<InnerClient> {
        &self.inner
    }

    /// Whether a stale-cache replay may be ATTEMPTED for an operation starting
    /// now.
    ///
    /// This rules out only what it can PROVE: a session PostgreSQL has already
    /// reported to be inside a transaction block, where `0A000` aborts the
    /// block and no reprepare can heal it. It deliberately does NOT reject the
    /// `None` that [`InnerClient::transaction_status`] returns while a request
    /// is in flight. `None` means "ask again after the next round trip", and
    /// reading it as "unsafe" here disabled the recovery for every pipelined or
    /// concurrently used connection -- the gate is evaluated BEFORE this
    /// operation is even sent, so an unrelated outstanding request said nothing
    /// about whether THIS one would run inside a transaction. What resolves it
    /// is `reprepare_cached_statement_once`, which owns an authoritative answer
    /// before anything is replayed.
    fn stale_cache_replay_permitted(&self) -> bool {
        !matches!(
            self.inner.transaction_status(),
            Some(TransactionStatus::InTransaction | TransactionStatus::Failed)
        )
    }

    /// Send a bare `Sync` and report the transaction status carried by ITS OWN
    /// `ReadyForQuery`.
    ///
    /// [`InnerClient::transaction_status`] reads a byte the connection task
    /// shares across every request, and answers `None` whenever any of them is
    /// still in flight. That is the right answer for "where is this session
    /// now", and the wrong instrument for this barrier, which needs the status
    /// at a point after THIS operation's failure. The status byte below rides
    /// the `ReadyForQuery` terminating this very `Sync`, so concurrent work on
    /// the connection cannot mask it.
    async fn sync_transaction_status(&self) -> Result<TransactionStatus, Error> {
        let mut responses = self
            .inner
            .send(RequestMessages::Single(FrontendMessage::Raw(
                Bytes::from_static(b"S\0\0\0\x04"),
            )))?;
        match responses.next().await? {
            Message::ReadyForQuery(body) => Ok(TransactionStatus::from_byte(body.status())),
            _ => Err(Error::unexpected_message()),
        }
    }

    /// Return one freshly resolved replacement when an implicit cache entry
    /// failed before producing caller-visible output.
    ///
    /// The Sync is a FIFO barrier for the failed request's trailing
    /// ReadyForQuery. Without it, the status could still describe the state
    /// before PostgreSQL aborted an open transaction.
    async fn reprepare_cached_statement_once(
        &self,
        cache_sql: Option<&str>,
        replay_permitted: bool,
        error: &Error,
    ) -> Option<Result<Statement, Error>> {
        let sql =
            cache_sql.filter(|_| replay_permitted && cached_statement_error_can_retry(error))?;
        if !matches!(
            self.sync_transaction_status().await,
            Ok(TransactionStatus::Idle)
        ) {
            // No retry attempt began. Keep the original server diagnostic
            // when the barrier cannot prove that replay is safe.
            return None;
        }

        // Deliberately return one replacement instead of looping: if its
        // attempt fails too, the condition is persistent or raced again and
        // the second server error is the useful result for the caller.
        Some(prepare::prepare_cached(self.inner(), sql).await)
    }

    pub(crate) fn tx_status_handle(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.inner.tx_status)
    }

    pub(crate) fn in_flight_requests_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.inner.in_flight_requests)
    }

    pub(crate) fn parameters_handle(&self) -> Arc<Mutex<HashMap<String, String>>> {
        Arc::clone(&self.inner.parameters)
    }

    /// The server's current value for a runtime parameter, or `None` if the
    /// server has never reported one.
    ///
    /// This is libpq's `PQparameterStatus`. PostgreSQL sends `ParameterStatus`
    /// for a hard-wired set of parameters -- 15 of them as of PostgreSQL 18,
    /// listed under "Asynchronous Operations" in the protocol-flow chapter --
    /// once at startup and again whenever the value changes, so this tracks the
    /// session rather than snapshotting it.
    ///
    /// The set is the server's to choose and is NOT the same as "settings that
    /// exist": `application_name`, `client_encoding`, `DateStyle`,
    /// `IntervalStyle`, `TimeZone`, `server_encoding`, `server_version`,
    /// `integer_datetimes`, `standard_conforming_strings`, `is_superuser`,
    /// `session_authorization`, `in_hot_standby`, `default_transaction_read_only`,
    /// `scram_iterations` and `search_path` are reported; `work_mem` and the
    /// several hundred other GUCs are not. A parameter the server does not
    /// report reads `None` however it was set, and `SHOW` is the way to ask
    /// about those -- at the cost of a round trip, which is the whole reason
    /// this exists.
    ///
    /// The set grows across releases, so treat a `None` as "this server does
    /// not report it" rather than "unset". `search_path` is the current
    /// example: PostgreSQL 18 added it (release notes, Libpq section), so on 16
    /// and 17 it reads `None` while very much having a value.
    ///
    /// Returns an owned `String` because the map is shared with the connection
    /// task and the lock cannot outlive the call.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<String> {
        self.inner.parameters.lock().get(name).cloned()
    }

    /// Used by this module's own tests; see the note on `Client::new`.
    #[cfg(test)]
    pub(crate) fn has_in_flight_requests(&self) -> bool {
        self.inner.has_in_flight_requests()
    }

    pub(crate) fn set_socket_config(&mut self, socket_config: SocketConfig) {
        self.socket_config = Some(socket_config);
    }

    /// Installs query execution observation for this physical connection.
    ///
    /// The returned unbounded receiver yields one completion event for each
    /// SQL execution enqueued after installation. Calling this again replaces
    /// the observer for future requests; already in-flight requests retain the
    /// receiver that observed their start.
    ///
    /// The driver only performs a non-blocking channel send. It never invokes
    /// caller code from the connection task, so an async consumer may issue a
    /// query on this client without re-entering that task. Because the channel
    /// is unbounded, callers must keep draining it to bound memory use.
    ///
    /// Install before preparing statements whose later executions need their
    /// SQL reported. Observation is intentionally disabled by default.
    #[must_use = "the receiver must be retained to observe query events"]
    pub fn query_events(&self) -> mpsc::UnboundedReceiver<QueryEvent> {
        let (sender, receiver) = mpsc::unbounded();
        self.inner.install_query_observer(sender, None);
        receiver
    }

    /// Installs query execution observation with a minimum elapsed duration.
    ///
    /// Only executions whose elapsed time is greater than or equal to
    /// `threshold` are reported. The cutoff applies equally to successful,
    /// failed, and cancelled executions. A zero threshold reports every
    /// execution and is equivalent to [`Client::query_events`].
    /// Elapsed time has the same enqueue-to-terminal-server-response meaning
    /// as [`QueryEvent::elapsed`]; it is not PostgreSQL execution time alone.
    ///
    /// Calling this method follows the same replacement and in-flight request
    /// semantics as [`Client::query_events`].
    ///
    /// A threshold discards every sub-threshold execution, so it cannot see a
    /// problem made of many individually fast queries: an N+1 issuing thousands
    /// of quick statements produces no events at all. Elapsed time also counts
    /// waiting behind an earlier query on the same connection, so one trivial
    /// statement can be reported for another's slowness. Use
    /// [`Client::query_events`] when the question is "what is this connection
    /// doing", and a threshold when it is "which single query is slow".
    #[must_use = "the receiver must be retained to observe query events"]
    pub fn query_events_with_threshold(
        &self,
        threshold: Duration,
    ) -> mpsc::UnboundedReceiver<QueryEvent> {
        let (sender, receiver) = mpsc::unbounded();
        self.inner.install_query_observer(sender, Some(threshold));
        receiver
    }

    /// Creates a new prepared statement.
    ///
    /// Prepared statements can be executed repeatedly, and may contain query parameters (indicated by `$1`, `$2`, etc),
    /// which are set when executed. Prepared statements can only be used with the connection that created them.
    pub async fn prepare(&self, query: &str) -> Result<Statement, Error> {
        self.prepare_typed(query, &[]).await
    }

    /// Like `prepare`, but allows the types of query parameters to be explicitly specified.
    ///
    /// The list of types may be smaller than the number of parameters - the types of the remaining parameters will be
    /// inferred. For example, `client.prepare_typed(query, &[])` is equivalent to `client.prepare(query)`.
    pub async fn prepare_typed(
        &self,
        query: &str,
        parameter_types: &[Type],
    ) -> Result<Statement, Error> {
        prepare::prepare(&self.inner, query, parameter_types).await
    }

    /// Executes a statement, returning a vector of the resulting rows.
    ///
    /// A statement may contain parameters, specified by `$n`, where `n` is the index of the parameter of the list
    /// provided, 1-indexed.
    ///
    /// The `statement` argument can either be a `Statement`, or a raw query string. If the same statement will be
    /// repeatedly executed (perhaps with different query parameters), consider preparing the statement up front
    /// with the `prepare` method.
    pub async fn query<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.query_raw(statement, slice_iter(params))
            .await?
            .try_collect()
            .await
    }

    /// Returns a vector of scalars.
    pub async fn query_scalar<R: FromSqlOwned, T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<R>, Error>
    where
        T: ?Sized + ToStatement + fmt::Debug,
    {
        // The arity comes from the STATEMENT, not from `rows.first()`. Reading
        // it off the first row made the verdict depend on the data: `SELECT
        // 1, 2` errored while `SELECT 1, 2 WHERE false` returned an empty Vec,
        // so a caller whose fixture happened to be empty got a green and met
        // the error once real rows existed.
        //
        // The stream is drained before the error is returned rather than
        // dropped early. This is a caller mistake and so a cold path; draining
        // keeps it off the abandoned-response machinery entirely.
        let stream = self.query_raw(statement, slice_iter(params)).await?;
        let column_count = stream.columns().len();
        let rows: Vec<Row> = stream.try_collect().await?;

        if column_count != 1 {
            return Err(Error::column_count());
        }

        rows.into_iter().map(|r| r.try_get(0)).collect()
    }

    /// Executes a statement which returns a single row, returning it.
    ///
    /// Returns an error if the query does not return exactly one row.
    pub async fn query_one<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.query_opt(statement, params)
            .await
            .and_then(|res| res.ok_or_else(Error::row_count))
    }

    /// Like [`Client::query_one`] but returns one scalar.
    pub async fn query_one_scalar<R: FromSqlOwned, T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<R, Error>
    where
        T: ?Sized + ToStatement + fmt::Debug,
    {
        // The arity comes from the STATEMENT, as in the two helpers above, and
        // for the same reason: read off a row it becomes data-dependent.
        //
        // Nothing was ever ACCEPTED here that should not have been - `query_one`
        // guarantees a row exists, so the check always ran. What varied was the
        // DIAGNOSTIC. Measured before this change, on one query whose column
        // list is wrong either way:
        //
        //   SELECT 1::int4, 2::int4 WHERE false -> "unexpected number of rows"
        //   SELECT 1::int4, 2::int4             -> "unexpected number of columns"
        //
        // The first blames the data for a mistake in the query text. Selecting
        // two columns is wrong whether or not the table has rows, so the column
        // verdict is the one that holds, and it now comes first.
        let stream = self.query_raw(statement, slice_iter(params)).await?;
        let column_count = stream.columns().len();
        let rows: Vec<Row> = stream.try_collect().await?;

        if column_count != 1 {
            return Err(Error::column_count());
        }
        if rows.len() != 1 {
            return Err(Error::row_count());
        }

        rows.into_iter()
            .next()
            .ok_or_else(Error::row_count)?
            .try_get(0)
    }

    /// Executes a statements which returns zero or one rows, returning it.
    ///
    /// Returns an error if the query returns more than one row.
    pub async fn query_opt<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, Error>
    where
        T: ?Sized + ToStatement,
    {
        let mut stream = pin!(self.query_raw(statement, slice_iter(params)).await?);

        let mut first = None;
        while let Some(row) = stream.try_next().await? {
            if first.is_some() {
                return Err(Error::row_count());
            }

            first = Some(row);
        }

        Ok(first)
    }

    /// Like [`Client::query_opt`] but returns an optional scalar.
    pub async fn query_opt_scalar<R: FromSqlOwned, T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<R>, Error>
    where
        T: ?Sized + ToStatement + fmt::Debug,
    {
        // Same data-dependence as `query_scalar` had, for the same reason: with
        // no row there was nothing to read an arity off, so a two-column query
        // returning nothing was accepted.
        let stream = self.query_raw(statement, slice_iter(params)).await?;
        let column_count = stream.columns().len();
        let rows: Vec<Row> = stream.try_collect().await?;

        if column_count != 1 {
            return Err(Error::column_count());
        }
        if rows.len() > 1 {
            return Err(Error::row_count());
        }

        rows.into_iter()
            .next()
            .map(|x| x.try_get::<_, R>(0))
            .transpose()
    }

    /// The maximally flexible version of [`query`].
    ///
    /// [`query`]: #method.query
    pub async fn query_raw<T, P, I>(&self, statement: &T, params: I) -> Result<RowStream, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let execution = statement.__convert().into_statement(&self.inner).await?;
        if execution.unnamed_sql.is_some() {
            let params = params.into_iter().collect::<Vec<_>>();
            let execution = execution
                .finalize_probationary(&self.inner, params.len())
                .await?;
            if let Some(sql) = execution.unnamed_sql {
                let types = execution.statement.params().to_vec();
                // Reparse and Bind in one frontend batch. The earlier unnamed
                // Describe supplied parameter OIDs; this second Parse makes
                // the execution immune to concurrent users of PostgreSQL's
                // unnamed statement slot, while query_typed reads the current
                // row shape.
                return query::query_typed(&self.inner, sql, params.into_iter().zip(types)).await;
            }

            let Some(cache_sql) = execution.cache_sql else {
                return query::query(&self.inner, execution.statement, params).await;
            };
            let replay_permitted = self.stale_cache_replay_permitted();
            if !replay_permitted {
                return query::query(&self.inner, execution.statement, params).await;
            }

            let first = query::query_cached(
                &self.inner,
                execution.statement,
                params.iter().map(BorrowToSql::borrow_to_sql),
            )
            .await;
            return match first {
                Ok(stream) => Ok(stream),
                Err(error) => {
                    let Some(replacement) = self
                        .reprepare_cached_statement_once(Some(cache_sql), replay_permitted, &error)
                        .await
                    else {
                        return Err(error);
                    };
                    query::query_cached(
                        &self.inner,
                        replacement?,
                        params.iter().map(BorrowToSql::borrow_to_sql),
                    )
                    .await
                }
            };
        }
        let Some(cache_sql) = execution.cache_sql else {
            return query::query(&self.inner, execution.statement, params).await;
        };
        let replay_permitted = self.stale_cache_replay_permitted();
        if !replay_permitted {
            return query::query(&self.inner, execution.statement, params).await;
        }

        let params = params.into_iter().collect::<Vec<_>>();
        let first = query::query_cached(
            &self.inner,
            execution.statement,
            params.iter().map(BorrowToSql::borrow_to_sql),
        )
        .await;
        match first {
            Ok(stream) => Ok(stream),
            Err(error) => {
                let Some(replacement) = self
                    .reprepare_cached_statement_once(Some(cache_sql), replay_permitted, &error)
                    .await
                else {
                    return Err(error);
                };
                query::query_cached(
                    &self.inner,
                    replacement?,
                    params.iter().map(BorrowToSql::borrow_to_sql),
                )
                .await
            }
        }
    }

    /// Like `query`, but requires the types of query parameters to be explicitly specified.
    pub async fn query_typed(
        &self,
        query: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>, Error> {
        self.query_typed_raw(query, params.iter().map(|(v, t)| (*v, t.clone())))
            .await?
            .try_collect()
            .await
    }

    /// Run a query with text-format string parameters.
    ///
    /// Parse is sent with no type hints - the server infers each parameter's
    /// type from its position in the SQL. This matches the legacy
    /// `zeroship-pg` `query_text_params` API and is what JSON-driven query
    /// builders rely on (they pass everything as strings and expect
    /// `UPDATE t SET c = $1` to implicit-cast regardless of `c`'s column
    /// type). Results come back in binary format.
    ///
    /// ```no_run
    /// # async fn demo(client: &compio_postgres::Client) -> Result<(), compio_postgres::Error> {
    /// let rows = client
    ///     .query_text_params(
    ///         "SELECT * FROM users WHERE id = $1::uuid",
    ///         &["550e8400-e29b-41d4-a716-446655440000"],
    ///     )
    ///     .await?;
    /// # let _ = rows; Ok(()) }
    /// ```
    pub async fn query_text_params(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>, Error> {
        query::query_text_params(&self.inner, sql, params)
            .await?
            .try_collect()
            .await
    }

    /// Execute a non-row statement with NULL-aware **text-format** parameters,
    /// returning the affected-row count. The `execute` peer of
    /// [`query_text_params`](Self::query_text_params): the server infers each
    /// parameter's type from its SQL position and a text value implicit-casts to
    /// the target column type - the coercion model a schema-blind DML assembler
    /// (op.* section 3.3) needs. A `None` element is a SQL NULL.
    pub async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, Error> {
        query::execute_text_params(&self.inner, sql, params).await
    }

    /// Like `query_one`, but requires the types of query parameters to be explicitly specified.
    pub async fn query_typed_one(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Row, Error> {
        self.query_typed_opt(statement, params)
            .await
            .and_then(|res| res.ok_or_else(Error::row_count))
    }

    /// Like `query_opt`, but requires the types of query parameters to be explicitly specified.
    pub async fn query_typed_opt(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Option<Row>, Error> {
        let mut stream = pin!(
            self.query_typed_raw(statement, params.iter().map(|(v, t)| (*v, t.clone())))
                .await?
        );

        let mut first = None;
        while let Some(row) = stream.try_next().await? {
            if first.is_some() {
                return Err(Error::row_count());
            }

            first = Some(row);
        }

        Ok(first)
    }

    /// The maximally flexible version of [`query_typed`].
    ///
    /// [`query_typed`]: #method.query_typed
    pub async fn query_typed_raw<P, I>(&self, query: &str, params: I) -> Result<RowStream, Error>
    where
        P: BorrowToSql,
        I: IntoIterator<Item = (P, Type)>,
    {
        query::query_typed(&self.inner, query, params).await
    }

    /// Executes a statement, returning the number of rows modified.
    pub async fn execute<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.execute_raw(statement, slice_iter(params)).await
    }

    /// Like `execute`, but requires the types of query parameters to be explicitly specified.
    pub async fn execute_typed(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<u64, Error> {
        query::execute_typed(
            &self.inner,
            statement,
            params.iter().map(|(v, t)| (*v, t.clone())),
        )
        .await
    }

    /// The maximally flexible version of [`execute`].
    ///
    /// [`execute`]: #method.execute
    pub async fn execute_raw<T, P, I>(&self, statement: &T, params: I) -> Result<u64, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let execution = statement.__convert().into_statement(&self.inner).await?;
        if execution.unnamed_sql.is_some() {
            let params = params.into_iter().collect::<Vec<_>>();
            let execution = execution
                .finalize_probationary(&self.inner, params.len())
                .await?;
            if let Some(sql) = execution.unnamed_sql {
                let types = execution.statement.params().to_vec();
                return query::execute_typed(&self.inner, sql, params.into_iter().zip(types)).await;
            }

            let Some(cache_sql) = execution.cache_sql else {
                return query::execute(self.inner(), execution.statement, params).await;
            };
            let replay_permitted = self.stale_cache_replay_permitted();
            if !replay_permitted {
                return query::execute(self.inner(), execution.statement, params).await;
            }

            let first = query::execute(
                self.inner(),
                execution.statement,
                params.iter().map(BorrowToSql::borrow_to_sql),
            )
            .await;
            return match first {
                Ok(rows) => Ok(rows),
                Err(error) => {
                    let Some(replacement) = self
                        .reprepare_cached_statement_once(Some(cache_sql), replay_permitted, &error)
                        .await
                    else {
                        return Err(error);
                    };
                    query::execute(
                        self.inner(),
                        replacement?,
                        params.iter().map(BorrowToSql::borrow_to_sql),
                    )
                    .await
                }
            };
        }
        let Some(cache_sql) = execution.cache_sql else {
            return query::execute(self.inner(), execution.statement, params).await;
        };
        let replay_permitted = self.stale_cache_replay_permitted();
        if !replay_permitted {
            return query::execute(self.inner(), execution.statement, params).await;
        }

        let params = params.into_iter().collect::<Vec<_>>();
        let first = query::execute(
            self.inner(),
            execution.statement,
            params.iter().map(BorrowToSql::borrow_to_sql),
        )
        .await;
        match first {
            Ok(rows) => Ok(rows),
            Err(error) => {
                let Some(replacement) = self
                    .reprepare_cached_statement_once(Some(cache_sql), replay_permitted, &error)
                    .await
                else {
                    return Err(error);
                };
                query::execute(
                    self.inner(),
                    replacement?,
                    params.iter().map(BorrowToSql::borrow_to_sql),
                )
                .await
            }
        }
    }

    /// Executes a `COPY FROM STDIN` statement, returning a sink used to write the copy data.
    ///
    /// PostgreSQL does not support parameters in `COPY` statements. The copy *must* be explicitly
    /// completed via the `Sink::close` or `finish` methods. If it is not, the copy will be aborted.
    pub async fn copy_in<T, U>(&self, statement: &T) -> Result<CopyInSink<U>, Error>
    where
        T: ?Sized + ToStatement,
        U: Buf + 'static + Send,
    {
        let execution = statement
            .__convert()
            .into_statement(&self.inner)
            .await?
            .finalize_probationary(&self.inner, 0)
            .await?;
        let replay_permitted = execution.cache_sql.is_some() && self.stale_cache_replay_permitted();
        match copy_in::copy_in(self.inner(), execution.statement, execution.unnamed_sql).await {
            Ok(sink) => Ok(sink),
            Err(error) => {
                let Some(replacement) = self
                    .reprepare_cached_statement_once(execution.cache_sql, replay_permitted, &error)
                    .await
                else {
                    return Err(error);
                };
                copy_in::copy_in(self.inner(), replacement?, None).await
            }
        }
    }

    /// Executes a `COPY TO STDOUT` statement, returning a stream of the resulting data.
    pub async fn copy_out<T>(&self, statement: &T) -> Result<CopyOutStream, Error>
    where
        T: ?Sized + ToStatement,
    {
        let execution = statement
            .__convert()
            .into_statement(&self.inner)
            .await?
            .finalize_probationary(&self.inner, 0)
            .await?;
        let replay_permitted = execution.cache_sql.is_some() && self.stale_cache_replay_permitted();
        match copy_out::copy_out(self.inner(), execution.statement, execution.unnamed_sql).await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                let Some(replacement) = self
                    .reprepare_cached_statement_once(execution.cache_sql, replay_permitted, &error)
                    .await
                else {
                    return Err(error);
                };
                copy_out::copy_out(self.inner(), replacement?, None).await
            }
        }
    }

    /// Executes a sequence of SQL statements using the simple query protocol, returning the resulting rows.
    ///
    /// Statements should be separated by semicolons. If an error occurs, execution of the sequence will stop at that
    /// point. The simple query protocol returns the values in rows as strings rather than in their binary encodings,
    /// so the associated row type doesn't work with the `FromSql` trait.
    pub async fn simple_query(&self, query: &str) -> Result<Vec<SimpleQueryMessage>, Error> {
        self.simple_query_raw(query).await?.try_collect().await
    }

    /// Like `simple_query`, but returns a stream rather than eagerly collecting.
    pub async fn simple_query_raw(&self, query: &str) -> Result<SimpleQueryStream, Error> {
        simple_query::simple_query(self.inner(), query).await
    }

    /// Executes a sequence of SQL statements using the simple query protocol.
    ///
    /// Statements should be separated by semicolons. If an error occurs, execution of the sequence will stop at that
    /// point. This is intended for use when, for example, initializing a database schema.
    pub async fn batch_execute(&self, query: &str) -> Result<(), Error> {
        simple_query::batch_execute(self.inner(), query).await
    }

    /// Check that the connection is alive and wait for the confirmation.
    pub async fn check_connection(&self) -> Result<(), Error> {
        query::sync(self.inner()).await
    }

    /// Begins a new database transaction.
    ///
    /// The transaction will roll back by default - use the `commit` method to commit it.
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        self.build_transaction().start().await
    }

    /// Returns a builder for a transaction with custom settings.
    pub fn build_transaction(&mut self) -> TransactionBuilder<'_> {
        TransactionBuilder::new(self)
    }

    /// Returns the process ID of the server backend connected to this
    /// client.
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    /// The wire protocol version this session SETTLED ON.
    ///
    /// Not the same as [`crate::Config::max_protocol_version`], which is only
    /// what was requested: a server that cannot speak it answers
    /// `NegotiateProtocolVersion` and the session continues one version down,
    /// so a connection asking for 3.2 reports 3.0 against PostgreSQL 15 or 16.
    ///
    /// This is the only way to find out. PostgreSQL exposes NO server-side view
    /// of the negotiated version - `pg_stat_activity` has no such column and
    /// `pg_settings` carries only the TLS `ssl_min/max_protocol_version` - so
    /// the client's own record is what `psql`'s `\conninfo` prints too.
    ///
    /// Worth checking when a cancel key is in question: 3.0 fixes it at four
    /// bytes and 3.2 makes it variable-length.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    /// Constructs a cancellation token that can later be used to request
    /// cancellation of a query running on this connection.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken {
            socket_config: self.socket_config.clone(),
            ssl_mode: self.ssl_mode,
            ssl_negotiation: self.ssl_negotiation,
            process_id: self.process_id,
            secret_key: self.secret_key.clone(),
        }
    }

    /// Attempts to cancel an in-progress query.
    ///
    /// The server provides no information about whether a cancellation attempt was successful or not. An error will
    /// only be returned if the client was unable to connect to the database.
    #[deprecated(since = "0.6.0", note = "use Client::cancel_token() instead")]
    pub async fn cancel_query<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        self.cancel_token().cancel_query(tls).await
    }

    /// Like `cancel_query`, but uses a stream which is already connected to the server rather than opening a new
    /// connection itself.
    #[deprecated(since = "0.6.0", note = "use Client::cancel_token() instead")]
    pub async fn cancel_query_raw<S, T>(&self, stream: S, tls: T) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        self.cancel_token().cancel_query_raw(stream, tls).await
    }

    /// Clears the client's type information cache.
    ///
    /// When user-defined types are used in a query, the client loads their definitions from the database and caches
    /// them for the lifetime of the client. If those definitions are changed in the database, this method can be used
    /// to flush the local cache and allow the new, updated definitions to be loaded. Implicitly cached statements
    /// which contain user-defined parameter or result types are also evicted so later raw-SQL operations reprepare
    /// them with the updated definitions.
    ///
    /// Statements returned by [`Client::prepare`] own their type information. Callers must prepare those statements
    /// again after clearing the cache; existing statements and rows are not changed in place.
    pub fn clear_type_cache(&self) {
        self.inner().clear_type_cache();
    }

    /// Determines if the connection to the server has already closed.
    /// In that case, all future queries will fail.
    pub fn is_closed(&self) -> bool {
        self.inner.sender.is_closed()
    }

    /// The transaction state the server reported in the last `ReadyForQuery`
    /// on this connection, or `None` if that answer is not settled yet.
    ///
    /// The connection task records the status in server wire order, whether or
    /// not response streams are polled, retained or dropped. But it records it
    /// when IT consumes the `ReadyForQuery`, and that is not the moment your
    /// `await` returns: an `ErrorResponse` and its trailing `ReadyForQuery` can
    /// arrive in different batches, so a failed statement can hand you its
    /// `SQLSTATE` before the task has seen the terminator.
    ///
    /// `None` IS THE POINT. Returning a stale `Idle` there is the bug this
    /// signature exists to prevent - a caller checking "am I in a transaction?"
    /// after an error would be told no, skip its rollback, and hand the next
    /// user of the connection an aborted transaction. Treat `None` as "ask
    /// again after the next round trip", not as "idle".
    #[must_use]
    pub fn transaction_status(&self) -> Option<TransactionStatus> {
        self.inner.transaction_status()
    }

    /// Is a fire-and-forget command (e.g. ROLLBACK from `Transaction::drop`)
    /// pending on this client? Used by the pool to decide whether a checkout
    /// barrier is required.
    pub fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }

    /// Clear the dirty flag. The pool calls this after a checkout barrier
    /// (or after an explicit commit/rollback confirms the connection is in
    /// a clean state).
    pub(crate) fn clear_dirty(&self) {
        self.inner.clear_dirty();
    }

    #[doc(hidden)]
    pub fn __private_api_rollback(&self, name: Option<&str>) {
        // Mark the connection dirty *before* firing the rollback so that a
        // concurrent pool return sees the flag even if the send itself fails
        // (in which case `is_closed()` will also be true, but the flag
        // remains correct).
        self.inner.set_dirty();

        let buf = self.inner().with_buf(|buf| {
            // One spelling of "end this savepoint's scope", shared with
            // `Transaction::rollback`: the rollback-on-drop path has to leave
            // the server in the same state the explicit call would, or which
            // exit a caller took would decide whether a savepoint outlives it.
            let sql = match name {
                Some(name) => crate::transaction::rollback_savepoint(name),
                None => "ROLLBACK".to_string(),
            };
            // H6: Don't panic on NUL in savepoint names. `frontend::query`
            // returns Err if the SQL contains an interior NUL byte; in that
            // case we emit a log line and return an empty buffer, so no
            // ROLLBACK is queued and the connection stays dirty.
            //
            // This used to add "and the pool's next-get barrier will still
            // detect + evict it". IT WILL NOT, and the same wrong claim was
            // removed from `pool.rs` on 2026-08-23. That barrier is
            // `simple_query("")`, which returns Ok whenever every earlier
            // request reached its ReadyForQuery; an empty simple query
            // SUCCEEDS inside an open or aborted transaction, so the barrier
            // clears `dirty` on a session it never proved idle.
            //
            // What actually protects the next borrower is `return_client`'s
            // rollback, which is unconditional for any session that is not
            // provably `Idle`. Reaching this arm at all needs an interior NUL
            // in a savepoint name, which `SAVEPOINT` creation rejects first,
            // so no `Transaction` exists to drop.
            if let Err(e) = frontend::query(&sql, buf) {
                log::error!("compio-postgres: failed to encode ROLLBACK: {e}");
                buf.clear();
            }
            buf.split().freeze()
        });

        // Empty buf => encoding failed; skip the send (nothing to send). The
        // dirty flag remains set so the pool will run the barrier on the
        // next checkout.
        if buf.is_empty() {
            return;
        }

        let _ = self.inner().send_with(
            RequestMessages::Single(FrontendMessage::Raw(buf)),
            RequestDisposition::Housekeeping,
            TransactionEffect::MayChange,
        );
    }

    #[doc(hidden)]
    pub fn __private_api_close(&mut self) {
        self.inner.sender.close_channel()
    }

    /// Retire this physical session without waiting for the connection task.
    ///
    /// Pool command-timeout recovery calls this only when it cannot prove a
    /// trailing `ReadyForQuery` was drained. The synchronous socket shutdown
    /// prevents the still-running backend from outliving the poisoned lease.
    pub(crate) fn force_close(&self) {
        self.inner.sender.close_channel();
        if let Some(release) = &self.inner.release {
            release.shutdown();
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").finish()
    }
}
