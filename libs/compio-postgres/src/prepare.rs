// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Originates as a translation of tokio-postgres's `prepare.rs`. The core
// Parse + Describe + Sync and pg_catalog flow remains recognizable, while
// cancellation ownership, recursive-type cycle detection, cache races, and
// multirange resolution deliberately diverge and are documented inline. The
// recursive helpers (`prepare_rec`, `get_type_rec`) remain boxed futures
// because the recursion between `prepare` <-> `get_type` <-> `prepare_rec`
// cannot be expressed as a plain async fn.

use crate::client::{InnerClient, Responses, StatementCacheAdmission};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::error::SqlState;
use crate::types::{Field, Kind, Oid, Type};
use crate::{Column, Error, Statement};
use crate::{query, slice_iter};
use bytes::Bytes;
use fallible_iterator::FallibleIterator;
use futures_util::TryStreamExt;
use log::debug;
use parking_lot::Mutex;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

const TYPEINFO_QUERY: &str = "\
SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, t.typrelid
FROM pg_catalog.pg_type t
LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid
INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid
WHERE t.oid = $1
";

// Multiranges and pg_range.rngmultitypid weren't added until Postgres 14.
// This query is prepared only after pg_type reports typtype = 'm', so older
// servers never parse a catalog column they do not have.
const TYPEINFO_MULTIRANGE_QUERY: &str = "\
SELECT rngsubtype
FROM pg_catalog.pg_range
WHERE rngmultitypid = $1
";

// Range types weren't added until Postgres 9.2, so pg_range may not exist
const TYPEINFO_FALLBACK_QUERY: &str = "\
SELECT t.typname, t.typtype, t.typelem, NULL::OID, t.typbasetype, n.nspname, t.typrelid
FROM pg_catalog.pg_type t
INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid
WHERE t.oid = $1
";

const TYPEINFO_ENUM_QUERY: &str = "\
SELECT enumlabel
FROM pg_catalog.pg_enum
WHERE enumtypid = $1
ORDER BY enumsortorder
";

// Postgres 9.0 didn't have enumsortorder
const TYPEINFO_ENUM_FALLBACK_QUERY: &str = "\
SELECT enumlabel
FROM pg_catalog.pg_enum
WHERE enumtypid = $1
ORDER BY oid
";

const TYPEINFO_COMPOSITE_QUERY: &str = "\
SELECT attname, atttypid
FROM pg_catalog.pg_attribute
WHERE attrelid = $1
AND NOT attisdropped
AND attnum > 0
ORDER BY attnum
";

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrepareState {
    Pending,
    Parsed,
    Rejected,
    Cancelled,
    Finished,
}

/// Shared ownership decision for a `Parse` whose caller can disappear before
/// it receives the first backend message.
///
/// The connection task observes `ParseComplete` or `ErrorResponse` before it
/// hands that message to the caller. If the prepare future was cancelled
/// first, that task can therefore close only a name this Parse actually
/// acquired. If the caller observed `ParseComplete` first, the guard below
/// performs the same cleanup synchronously from its own `Drop`.
#[derive(Clone)]
pub(crate) struct PrepareCleanup(Arc<PrepareCleanupInner>);

struct PrepareCleanupInner {
    client: Weak<InnerClient>,
    name: String,
    state: Mutex<PrepareState>,
}

impl PrepareCleanup {
    fn new(client: &Arc<InnerClient>, name: &str) -> PrepareCleanup {
        PrepareCleanup(Arc::new(PrepareCleanupInner {
            client: Arc::downgrade(client),
            name: name.to_string(),
            state: Mutex::new(PrepareState::Pending),
        }))
    }

    /// Record a normal `ParseComplete` or `ErrorResponse` outcome. The
    /// connection task calls this even when the response receiver was already
    /// dropped.
    pub(crate) fn observe(&self, parsed: bool) {
        let close = {
            let mut state = self.0.state.lock();
            match (*state, parsed) {
                (PrepareState::Pending, true) => *state = PrepareState::Parsed,
                (PrepareState::Pending, false) => *state = PrepareState::Rejected,
                (PrepareState::Cancelled, _) => *state = PrepareState::Finished,
                _ => return,
            }
            parsed && *state == PrepareState::Finished
        };
        if close {
            self.close();
        }
    }

    /// Release a successfully parsed name to the returned Statement.
    fn disarm(&self) {
        let mut state = self.0.state.lock();
        debug_assert_eq!(*state, PrepareState::Parsed);
        *state = PrepareState::Finished;
    }

    /// Cancel the caller's interest. Cleanup is immediate if ParseComplete
    /// was already observed, deferred if its response is still in flight, and
    /// suppressed if PostgreSQL rejected the Parse.
    fn cancel(&self) {
        let close = {
            let mut state = self.0.state.lock();
            match *state {
                PrepareState::Pending => {
                    *state = PrepareState::Cancelled;
                    false
                }
                PrepareState::Parsed => {
                    *state = PrepareState::Finished;
                    true
                }
                PrepareState::Rejected => {
                    *state = PrepareState::Finished;
                    false
                }
                PrepareState::Cancelled | PrepareState::Finished => return,
            }
        };
        if close {
            self.close();
        }
    }

    fn close(&self) {
        if let Some(client) = self.0.client.upgrade() {
            crate::statement::close_statement(&client, &self.0.name);
        }
    }
}

/// Owns a statement name from the moment `Parse` is queued until a `Statement`
/// takes over responsibility for closing it.
///
/// `prepare` queues `Parse + Describe + Sync` and then awaits several
/// responses. `StatementInner::drop` is what sends `Close S`, and no
/// `Statement` exists until the whole exchange succeeds - so a future dropped
/// in that window (a cancelled caller, a timeout, an error out of the
/// `get_type` lookups) would leave the parsed statement on the server for the
/// rest of the session with nothing able to name it. This guard either closes
/// it or leaves that decision with the connection until Parse's outcome is
/// known; `disarm` hands the name over once a `Statement` is about to exist.
struct ParsedStatementGuard {
    cleanup: PrepareCleanup,
    name: Option<String>,
}

impl ParsedStatementGuard {
    /// Release the name to the caller, which is about to build the `Statement`
    /// whose `Drop` closes it from here on.
    fn disarm(mut self) -> String {
        self.cleanup.disarm();
        self.name
            .take()
            .expect("a guard holds its name until it is disarmed exactly once")
    }
}

impl Drop for ParsedStatementGuard {
    fn drop(&mut self) {
        if self.name.take().is_some() {
            self.cleanup.cancel();
        }
    }
}

pub async fn prepare(
    client: &Arc<InnerClient>,
    query: &str,
    types: &[Type],
) -> Result<Statement, Error> {
    let name = format!("s{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let buf = encode(client, &name, query, types)?;
    // Armed before the Parse is queued, because from that point on the server
    // may hold the statement and the shared cleanup is its only prospective
    // owner. Encoding failures above send nothing, so they need no guard.
    let cleanup = PrepareCleanup::new(client, &name);
    let guard = ParsedStatementGuard {
        cleanup: cleanup.clone(),
        name: Some(name),
    };
    let mut responses =
        client.send_prepare(RequestMessages::Single(FrontendMessage::Raw(buf)), cleanup)?;

    let (parameters, columns) = read_prepare_response(client, &mut responses).await?;
    Ok(Statement::new(
        client,
        guard.disarm(),
        parameters,
        columns,
        crate::simple_query::may_enter_copy_in(query),
    ))
}

/// Describe one execution without allocating a session-lived server name.
/// The operation reparses this SQL immediately before Bind, so this discovery
/// statement cannot be replaced incorrectly by a concurrent unnamed Parse.
async fn prepare_unnamed(
    client: &Arc<InnerClient>,
    query: &str,
    types: &[Type],
) -> Result<Statement, Error> {
    let buf = encode(client, "", query, types)?;
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;
    let (parameters, columns) = read_prepare_response(client, &mut responses).await?;
    Ok(Statement::unnamed_with_copy_in(
        parameters,
        columns,
        crate::simple_query::may_enter_copy_in(query),
    ))
}

async fn read_prepare_response(
    client: &Arc<InnerClient>,
    responses: &mut Responses,
) -> Result<(Vec<Type>, Vec<Column>), Error> {
    match responses.next().await? {
        Message::ParseComplete => {}
        _ => return Err(Error::unexpected_message()),
    }

    let parameter_description = match responses.next().await? {
        Message::ParameterDescription(body) => body,
        _ => return Err(Error::unexpected_message()),
    };

    let row_description = match responses.next().await? {
        Message::RowDescription(body) => Some(body),
        Message::NoData => None,
        _ => return Err(Error::unexpected_message()),
    };

    // ParseComplete and the two descriptions prove that PostgreSQL accepted
    // the Parse/Describe prefix, not that the Sync-terminated exchange
    // succeeded. A terminal ErrorResponse can follow that valid prefix; keep
    // the request alive through ReadyForQuery so it owns the prepare result.
    match responses.next().await? {
        Message::ReadyForQuery(_) => {}
        _ => return Err(Error::unexpected_message()),
    }

    let mut parameters = vec![];
    let mut it = parameter_description.parameters();
    while let Some(oid) = it.next().map_err(Error::parse)? {
        let type_ = get_type(client, oid).await?;
        parameters.push(type_);
    }

    let mut columns = vec![];
    if let Some(row_description) = row_description {
        let mut it = row_description.fields();
        while let Some(field) = it.next().map_err(Error::parse)? {
            let type_ = get_type(client, field.type_oid()).await?;
            let column = Column {
                name: field.name().to_string(),
                table_oid: Some(field.table_oid()).filter(|n| *n != 0),
                column_id: Some(field.column_id()).filter(|n| *n != 0),
                type_modifier: field.type_modifier(),
                r#type: type_,
            };
            columns.push(column);
        }
    }

    Ok((parameters, columns))
}

/// Prepare an implicit raw-SQL statement through this connection's opt-in
/// exact-text cache. Public `Client::prepare` stays outside this path: an
/// explicitly prepared Statement remains a distinct caller-owned object.
pub(crate) async fn prepare_cached(
    client: &Arc<InnerClient>,
    query: &str,
) -> Result<Statement, Error> {
    if client.statement_cache_capacity() == 0 {
        return prepare(client, query, &[]).await;
    }
    if let Some(statement) = client.cached_statement(query) {
        return Ok(statement);
    }
    Ok(prepare_and_cache(client, query).await?.0)
}

pub(crate) struct CachedStatement {
    pub(crate) statement: Statement,
    pub(crate) cache_hit: bool,
    pub(crate) unnamed: bool,
}

/// Prepare through the implicit cache while retaining whether the returned
/// Statement was a cache winner rather than this call's cold candidate. Only
/// a winner is stale-cache retry eligible; a fresh candidate's first failure
/// belongs to that prepare.
pub(crate) async fn prepare_cached_with_origin(
    client: &Arc<InnerClient>,
    query: &str,
) -> Result<CachedStatement, Error> {
    if client.statement_cache_capacity() == 0 {
        return Ok(CachedStatement {
            statement: prepare(client, query, &[]).await?,
            cache_hit: false,
            unnamed: false,
        });
    }
    if let Some(statement) = client.cached_statement(query) {
        return Ok(CachedStatement {
            statement,
            cache_hit: true,
            unnamed: false,
        });
    }
    // Discover parameter and result types without earning execution credit.
    // The operation finalizes admission only after its local arity check, so a
    // wrong-arity call cannot push SQL toward promotion.
    //
    // A threshold of one - the default - takes this path too, and used to have
    // a fast path above that prepared and cached before the arity check. That
    // let a call the caller got wrong install a connection-lived statement.
    // Removing it does NOT weaken `statement_cache_execution_threshold`'s
    // documented "preserving immediate preparation": admission still lands
    // before the operation executes, it just now follows validation instead of
    // preceding it, and `statement_cache_execution_threshold_one_promotes_
    // immediately` still passes.
    Ok(CachedStatement {
        statement: prepare_unnamed(client, query, &[]).await?,
        cache_hit: false,
        unnamed: true,
    })
}

/// Count one validated use of a probationary SQL string and choose its final
/// protocol path. Discovery happens outside the admission lock, so another
/// operation may have installed a cache winner in the meantime.
pub(crate) async fn finalize_probationary(
    client: &Arc<InnerClient>,
    query: &str,
    unnamed: Statement,
) -> Result<CachedStatement, Error> {
    match client.statement_cache_admission(query) {
        StatementCacheAdmission::Cached(statement) => Ok(CachedStatement {
            statement,
            cache_hit: true,
            unnamed: false,
        }),
        StatementCacheAdmission::PrepareNamed => {
            let (statement, cache_hit) = prepare_and_cache(client, query).await?;
            Ok(CachedStatement {
                statement,
                cache_hit,
                unnamed: false,
            })
        }
        StatementCacheAdmission::ExecuteUnnamed => Ok(CachedStatement {
            statement: unnamed,
            cache_hit: false,
            unnamed: true,
        }),
    }
}

/// Prepare an admitted SQL string and elect one cache winner. Stale-plan
/// recovery calls this path directly because a previously admitted statement
/// must not return to probation merely because PostgreSQL invalidated it.
async fn prepare_and_cache(
    client: &Arc<InnerClient>,
    query: &str,
) -> Result<(Statement, bool), Error> {
    let type_cache_generation = client.type_cache_generation();
    let statement = prepare(client, query, &[]).await?;
    let candidate = statement.clone();
    let statement = client.cache_statement(query, statement, type_cache_generation);
    let cache_hit = !statement.same_instance(&candidate);
    Ok((statement, cache_hit))
}

/// Build an error describing a cycle in pg_catalog type resolution.
///
/// The Error type has no dedicated `cycle_detected`/`unknown_type`
/// constructor and this file is not allowed to modify the error module,
/// so we reuse `Error::parse` (which wraps an `io::Error`) to surface the
/// cycle with a descriptive message. Semantically the failure is "we
/// cannot resolve this type from pg_catalog" - close enough to a parse
/// error over the type-info response.
fn cycle_detected(oid: Oid) -> Error {
    Error::parse(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("cycle detected resolving postgres type with OID {oid}"),
    ))
}

fn prepare_rec<'a>(
    client: &'a Arc<InnerClient>,
    query: &'a str,
    types: &'a [Type],
) -> Pin<Box<dyn Future<Output = Result<Statement, Error>> + 'a + Send>> {
    Box::pin(prepare(client, query, types))
}

fn encode(client: &InnerClient, name: &str, query: &str, types: &[Type]) -> Result<Bytes, Error> {
    if types.is_empty() {
        debug!("preparing query {name}: {query}");
    } else {
        debug!("preparing query {name} with types {types:?}: {query}");
    }

    client.with_buf(|buf| {
        frontend::parse(name, query, types.iter().map(Type::oid), buf).map_err(Error::encode)?;
        frontend::describe(b'S', name, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })
}

pub(crate) async fn get_type(client: &Arc<InnerClient>, oid: Oid) -> Result<Type, Error> {
    let mut in_flight = HashSet::new();
    get_type_inner(client, oid, &mut in_flight).await
}

/// Resolve a type OID, threading an `in_flight` set through the recursion
/// to detect cycles. A cycle can occur when a domain type is defined over
/// itself, or a composite type transitively references its own row type.
/// Without this guard, resolution recurses forever because `client.cached_type`
/// only returns `Some` after `set_type` fires - so any OID currently
/// being resolved is invisible to nested callers.
///
/// THERE IS NO LIVE TEST FOR THIS, AND THAT IS NOT AN OVERSIGHT. A well-behaved
/// server will not serve a catalog containing such a cycle: PostgreSQL refuses
/// to build one, transitively. MEASURED 2026-08-26 on 16.15 - a composite
/// gaining an attribute of its own type, of an ARRAY of its own type, and a
/// mutual cycle across two composites are each rejected with `composite type
/// <t> cannot be made a member of itself`, and a domain over itself cannot be
/// created because the name does not exist yet. So this guard defends against a
/// hostile or corrupted catalog rather than an ordinary one, in the same spirit
/// as `tests/suite/hostile_peer.rs`, and reaching it from live DDL is not
/// possible. Do not go looking for the missing test.
async fn get_type_inner(
    client: &Arc<InnerClient>,
    oid: Oid,
    in_flight: &mut HashSet<Oid>,
) -> Result<Type, Error> {
    if let Some(type_) = Type::from_oid(oid) {
        return Ok(type_);
    }

    let (cached, generation) = client.cached_type(oid);
    if let Some(type_) = cached {
        return Ok(type_);
    }

    if !in_flight.insert(oid) {
        return Err(cycle_detected(oid));
    }

    // `oid` must be removed from `in_flight` on every exit path so a
    // later retry (e.g. after the cycle-bearing type is dropped) can
    // succeed. Run the resolution in a nested async block, then always
    // remove before returning.
    let result = get_type_body(client, oid, in_flight, generation).await;
    in_flight.remove(&oid);
    result
}

/// Body of `get_type_inner` - separated so the parent can guarantee
/// `in_flight.remove(&oid)` runs on every return path. The caller is
/// responsible for inserting `oid` into `in_flight` before calling.
async fn get_type_body(
    client: &Arc<InnerClient>,
    oid: Oid,
    in_flight: &mut HashSet<Oid>,
    generation: u64,
) -> Result<Type, Error> {
    let stmt = typeinfo_statement(client).await?;

    // The catalog predicate is unique, but its first DataRow is not the
    // terminal result of Execute. Drain through ReadyForQuery before decoding
    // or caching anything so a later ErrorResponse cannot lose to a local row
    // conversion or to this function returning early.
    let row = drain_single_row(query::query(client, stmt, slice_iter(&[&oid])).await?).await?;

    let name: String = row.try_get(0)?;
    let type_: i8 = row.try_get(1)?;
    let elem_oid: Oid = row.try_get(2)?;
    let rngsubtype: Option<Oid> = row.try_get(3)?;
    let basetype: Oid = row.try_get(4)?;
    let schema: String = row.try_get(5)?;
    let relid: Oid = row.try_get(6)?;

    let kind = if type_ == b'e' as i8 {
        let variants = get_enum_variants(client, oid).await?;
        Kind::Enum(variants)
    } else if type_ == b'p' as i8 {
        Kind::Pseudo
    } else if basetype != 0 {
        let type_ = get_type_rec(client, basetype, in_flight).await?;
        Kind::Domain(type_)
    } else if elem_oid != 0 {
        let type_ = get_type_rec(client, elem_oid, in_flight).await?;
        Kind::Array(type_)
    } else if relid != 0 {
        let fields = get_composite_fields(client, relid, in_flight).await?;
        Kind::Composite(fields)
    } else if type_ == b'm' as i8 {
        let rngsubtype = get_multirange_subtype(client, oid).await?;
        let type_ = get_type_rec(client, rngsubtype, in_flight).await?;
        Kind::Multirange(type_)
    } else if let Some(rngsubtype) = rngsubtype {
        let type_ = get_type_rec(client, rngsubtype, in_flight).await?;
        Kind::Range(type_)
    } else {
        Kind::Simple
    };

    let type_ = Type::new(name, oid, kind, schema);
    client.set_type(oid, &type_, generation);

    Ok(type_)
}

fn get_type_rec<'a>(
    client: &'a Arc<InnerClient>,
    oid: Oid,
    in_flight: &'a mut HashSet<Oid>,
) -> Pin<Box<dyn Future<Output = Result<Type, Error>> + Send + 'a>> {
    Box::pin(get_type_inner(client, oid, in_flight))
}

async fn drain_single_row(rows: query::RowStream) -> Result<crate::Row, Error> {
    let mut rows = pin!(rows);
    let mut first = None;
    let mut multiple = false;

    while let Some(row) = rows.try_next().await? {
        if first.is_none() {
            first = Some(row);
        } else {
            multiple = true;
        }
    }

    match (first, multiple) {
        (Some(row), false) => Ok(row),
        _ => Err(Error::unexpected_message()),
    }
}

async fn get_multirange_subtype(client: &Arc<InnerClient>, oid: Oid) -> Result<Oid, Error> {
    let stmt = prepare_rec(client, TYPEINFO_MULTIRANGE_QUERY, &[]).await?;
    let row = drain_single_row(query::query(client, stmt, slice_iter(&[&oid])).await?).await?;
    row.try_get(0)
}

async fn typeinfo_statement(client: &Arc<InnerClient>) -> Result<Statement, Error> {
    if let Some(stmt) = client.typeinfo() {
        return Ok(stmt);
    }

    let stmt = match prepare_rec(client, TYPEINFO_QUERY, &[]).await {
        Ok(stmt) => stmt,
        Err(ref e) if e.code() == Some(&SqlState::UNDEFINED_TABLE) => {
            prepare_rec(client, TYPEINFO_FALLBACK_QUERY, &[]).await?
        }
        Err(e) => return Err(e),
    };

    // Recheck-before-set: two concurrent `query_raw` calls can both miss
    // the cache, both PREPARE on the server, and only one can win the
    // cache slot. If another task beat us, return its Statement and drop
    // ours - `StatementInner::drop` in statement.rs sends `Close S`, so
    // the extra server-side statement is DEALLOCATEd instead of leaking
    // until connection close.
    if let Some(other) = client.typeinfo() {
        return Ok(other);
    }
    client.set_typeinfo(&stmt);
    Ok(stmt)
}

async fn get_enum_variants(client: &Arc<InnerClient>, oid: Oid) -> Result<Vec<String>, Error> {
    let stmt = typeinfo_enum_statement(client).await?;

    query::query(client, stmt, slice_iter(&[&oid]))
        .await?
        .and_then(|row| async move { row.try_get(0) })
        .try_collect()
        .await
}

async fn typeinfo_enum_statement(client: &Arc<InnerClient>) -> Result<Statement, Error> {
    if let Some(stmt) = client.typeinfo_enum() {
        return Ok(stmt);
    }

    let stmt = match prepare_rec(client, TYPEINFO_ENUM_QUERY, &[]).await {
        Ok(stmt) => stmt,
        Err(ref e) if e.code() == Some(&SqlState::UNDEFINED_COLUMN) => {
            prepare_rec(client, TYPEINFO_ENUM_FALLBACK_QUERY, &[]).await?
        }
        Err(e) => return Err(e),
    };

    // Recheck-before-set: see `typeinfo_statement` for the rationale.
    if let Some(other) = client.typeinfo_enum() {
        return Ok(other);
    }
    client.set_typeinfo_enum(&stmt);
    Ok(stmt)
}

async fn get_composite_fields(
    client: &Arc<InnerClient>,
    oid: Oid,
    in_flight: &mut HashSet<Oid>,
) -> Result<Vec<Field>, Error> {
    let stmt = typeinfo_composite_statement(client).await?;

    let rows = query::query(client, stmt, slice_iter(&[&oid]))
        .await?
        .try_collect::<Vec<_>>()
        .await?;

    let mut fields = vec![];
    for row in rows {
        let name = row.try_get(0)?;
        let oid = row.try_get(1)?;
        let type_ = get_type_rec(client, oid, in_flight).await?;
        fields.push(Field::new(name, type_));
    }

    Ok(fields)
}

async fn typeinfo_composite_statement(client: &Arc<InnerClient>) -> Result<Statement, Error> {
    if let Some(stmt) = client.typeinfo_composite() {
        return Ok(stmt);
    }

    let stmt = prepare_rec(client, TYPEINFO_COMPOSITE_QUERY, &[]).await?;

    // Recheck-before-set: see `typeinfo_statement` for the rationale.
    if let Some(other) = client.typeinfo_composite() {
        return Ok(other);
    }
    client.set_typeinfo_composite(&stmt);
    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::{get_multirange_subtype, get_type, prepare};
    use crate::client::{Client, CopyMode, ResponseMessages, StatementCacheSettings};
    use crate::codec::{BackendMessages, FrontendMessage};
    use crate::config::{ProtocolVersion, SslMode, SslNegotiation};
    use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
    use crate::types::Type;
    use crate::{Column, Statement};
    use bytes::BytesMut;
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use futures_util::task::noop_waker;
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(body.len() + 5);
        frame.push(tag);
        frame.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 4).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn data_row(fields: &[Option<&[u8]>]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&u16::try_from(fields.len()).unwrap().to_be_bytes());
        for field in fields {
            match field {
                Some(bytes) => {
                    body.extend_from_slice(&i32::try_from(bytes.len()).unwrap().to_be_bytes());
                    body.extend_from_slice(bytes);
                }
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        body
    }

    fn oid_row_description(name: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&Type::OID.oid().to_be_bytes());
        body.extend_from_slice(&4i16.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body
    }

    fn column(name: &str, type_: Type) -> Column {
        Column {
            name: name.to_string(),
            table_oid: None,
            column_id: None,
            type_modifier: -1,
            r#type: type_,
        }
    }

    #[test]
    fn abandoned_prepare_keeps_its_deferred_close_during_copy() {
        let (sender, mut receiver) = mpsc::unbounded();
        let client = Client::new_with_statement_cache(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(0, NonZeroUsize::MIN),
        );
        let inner = Arc::clone(client.inner());

        let mut abandoned = Box::pin(prepare(&inner, "SELECT 1", &[]));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(
            matches!(abandoned.as_mut().poll(&mut context), Poll::Pending),
            "the prepare fixture completed before it could be abandoned"
        );
        drop(abandoned);

        let copy_statement = Statement::unnamed_with_copy_in(Vec::new(), Vec::new(), true);
        let (_copy_responses, _copy_guard) = inner
            .send_copy_statement(
                RequestMessages::Single(FrontendMessage::Raw(BytesMut::new().freeze())),
                &copy_statement,
                CopyMode::In,
            )
            .expect("claim COPY mode after abandoning prepare");

        let prepare_request = receiver
            .try_recv()
            .expect("the abandoned prepare did not enqueue its Parse");
        let _copy_request = receiver
            .try_recv()
            .expect("the COPY claim did not enqueue its request");
        prepare_request
            .prepare_cleanup
            .as_ref()
            .expect("the Parse request did not retain deferred cleanup")
            .observe(true);

        let close = receiver
            .try_recv()
            .expect("abandoned prepare lost its deferred Close while COPY was active");
        assert_eq!(close.disposition, RequestDisposition::Housekeeping);
        assert_eq!(close.transaction_effect, TransactionEffect::Neutral);
    }

    #[compio::test]
    async fn type_lookup_preserves_a_server_error_after_its_first_row() {
        const OID: u32 = 900_001;
        let (sender, mut receiver) = mpsc::unbounded();
        let client = Client::new_with_statement_cache(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(0, NonZeroUsize::MIN),
        );
        let inner = Arc::clone(client.inner());
        let statement = Statement::new(
            &inner,
            "s_scripted_typeinfo".to_string(),
            vec![Type::OID],
            vec![
                column("typname", Type::TEXT),
                column("typtype", Type::CHAR),
                column("typelem", Type::OID),
                column("rngsubtype", Type::OID),
                column("typbasetype", Type::OID),
                column("nspname", Type::NAME),
                column("typrelid", Type::OID),
            ],
            false,
        );
        inner.set_typeinfo(&statement);

        let lookup = get_type(&inner, OID);
        let respond = async {
            let mut request = receiver
                .next()
                .await
                .expect("type lookup did not enqueue its catalog query");
            let zero = 0u32.to_be_bytes();
            let row = data_row(&[
                Some(b"scripted_type"),
                Some(b"b"),
                Some(&zero),
                None,
                Some(&zero),
                Some(b"public"),
                Some(&zero),
            ]);
            let mut bytes = BytesMut::new();
            bytes.extend_from_slice(&backend_frame(b'2', b""));
            bytes.extend_from_slice(&backend_frame(b'D', &row));
            bytes.extend_from_slice(&backend_frame(
                b'E',
                b"SERROR\0VERROR\0C57014\0Mscripted cancellation after catalog row\0\0",
            ));
            bytes.extend_from_slice(&backend_frame(b'Z', b"I"));
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    bytes,
                )))
                .expect("deliver the terminal type lookup response");
        };

        let (result, ()) = futures_util::join!(lookup, respond);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("type lookup returned success before its cancellation SQLSTATE 57014"),
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57014"),
            "type lookup discarded cancellation SQLSTATE 57014: {error}"
        );
        assert!(
            inner.cached_type(OID).0.is_none(),
            "a type from a terminally failed lookup entered the cache"
        );
    }

    #[compio::test]
    async fn multirange_lookup_preserves_a_server_error_after_its_first_row() {
        const OID: u32 = 900_002;
        let (sender, mut receiver) = mpsc::unbounded();
        let client = Client::new_with_statement_cache(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            ProtocolVersion::V3_0,
            StatementCacheSettings::new(0, NonZeroUsize::MIN),
        );
        let inner = Arc::clone(client.inner());

        let lookup = get_multirange_subtype(&inner, OID);
        let respond = async {
            let mut prepare = receiver
                .next()
                .await
                .expect("multirange lookup did not prepare its catalog query");
            prepare
                .prepare_cleanup
                .as_ref()
                .expect("the internal prepare lacked its cleanup observer")
                .observe(true);

            let mut parameter_description = Vec::new();
            parameter_description.extend_from_slice(&1u16.to_be_bytes());
            parameter_description.extend_from_slice(&Type::OID.oid().to_be_bytes());
            let mut bytes = BytesMut::new();
            bytes.extend_from_slice(&backend_frame(b'1', b""));
            bytes.extend_from_slice(&backend_frame(b't', &parameter_description));
            bytes.extend_from_slice(&backend_frame(b'T', &oid_row_description("rngsubtype")));
            bytes.extend_from_slice(&backend_frame(b'Z', b"I"));
            prepare
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    bytes,
                )))
                .expect("deliver the multirange catalog prepare response");

            let mut query = receiver
                .next()
                .await
                .expect("multirange lookup did not enqueue its catalog query");
            let subtype = Type::INT4.oid().to_be_bytes();
            let mut bytes = BytesMut::new();
            bytes.extend_from_slice(&backend_frame(b'2', b""));
            bytes.extend_from_slice(&backend_frame(b'D', &data_row(&[Some(&subtype)])));
            bytes.extend_from_slice(&backend_frame(
                b'E',
                b"SERROR\0VERROR\0C57014\0Mscripted cancellation after multirange row\0\0",
            ));
            bytes.extend_from_slice(&backend_frame(b'Z', b"I"));
            query
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    bytes,
                )))
                .expect("deliver the terminal multirange lookup response");
        };

        let (result, ()) = futures_util::join!(lookup, respond);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!(
                "multirange subtype lookup returned success before its cancellation SQLSTATE 57014"
            ),
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57014"),
            "multirange subtype lookup discarded cancellation SQLSTATE 57014: {error}"
        );
    }
}
