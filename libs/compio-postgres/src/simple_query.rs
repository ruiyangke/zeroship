// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The simple-query framing remains upstream-derived, but request handling has
// diverged: COPY IN is aborted by a connection-owned producer and COPY OUT is
// drained before refusal. Transaction cleanup/tag helpers, bounded column
// reservation, and type-OID/format metadata are local extensions.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::command_tag::extract_row_affected;
use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
use crate::copy_in::CopyInReceiver;
use crate::{Error, SimpleQueryMessage, SimpleQueryRow};
use bytes::Bytes;
use fallible_iterator::FallibleIterator;
use futures_util::Stream;
use log::debug;
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

/// The wire format of a value returned by the simple query protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SimpleQueryFormat {
    /// PostgreSQL's text representation.
    Text,
    /// PostgreSQL's type-specific binary representation.
    Binary,
}

impl SimpleQueryFormat {
    fn from_code(code: i16) -> Result<Self, Error> {
        match code {
            0 => Ok(Self::Text),
            1 => Ok(Self::Binary),
            _ => Err(Error::parse(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("RowDescription carries invalid format code {code}"),
            ))),
        }
    }
}

/// Information about a column of a single query row.
#[derive(Debug)]
pub struct SimpleColumn {
    name: String,
    type_oid: u32,
    format: SimpleQueryFormat,
}

impl SimpleColumn {
    pub(crate) fn new(name: String, type_oid: u32, format: SimpleQueryFormat) -> SimpleColumn {
        SimpleColumn {
            name,
            type_oid,
            format,
        }
    }

    /// Returns the name of the column.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the PostgreSQL type OID of the column.
    ///
    /// This identifies the type-specific representation used by a
    /// [`SimpleQueryFormat::Binary`] value.
    pub fn type_oid(&self) -> u32 {
        self.type_oid
    }

    /// Returns the wire format of the column's values.
    pub fn format(&self) -> SimpleQueryFormat {
        self.format
    }
}

pub async fn simple_query(
    client: &Arc<InnerClient>,
    query: &str,
) -> Result<SimpleQueryStream, Error> {
    debug!("executing simple query: {query}");

    let must_prequeue_copy_abort = may_enter_copy_in(query);
    let buf = encode(client, query)?;
    let responses = client.send(producerless_request(buf, must_prequeue_copy_abort))?;

    Ok(SimpleQueryStream {
        responses,
        columns: None,
        copy_out_refused: false,
    })
}

pub async fn batch_execute(client: &InnerClient, query: &str) -> Result<(), Error> {
    let responses = start_batch_execute(client, query)?;
    finish_batch_execute(responses).await
}

/// The reason this driver gives PostgreSQL for aborting a copy it cannot feed.
///
/// It travels to the caller inside the server's own `COPY from stdin failed:`
/// error, which is what makes the failure attributable: an abandoned
/// [`crate::CopyInSink`] aborts the identical statement with an EMPTY reason,
/// so the server prefix alone cannot tell the two apart.
const COPY_IN_UNSUPPORTED: &str = "simple query execution cannot supply COPY data; use copy_in";

/// The reason an EXTENDED-protocol API gives for the same abort.
///
/// Spelled differently on purpose: it is the only thing that tells a caller
/// WHICH entry point could not feed the copy, and both paths reach the same
/// `COPY from stdin failed:` prefix.
pub(crate) const COPY_IN_UNSUPPORTED_EXTENDED: &str =
    "extended query execution cannot supply COPY data; use copy_in";

/// PostgreSQL refuses a target list wider than this, so no well-formed
/// `RowDescription` can describe more columns. Used to bound a reservation,
/// never to reject: a peer that sends more simply makes the Vec grow.
/// MEASURED on 16.14, 2026-08-26: 1664 columns succeed, 1665 fails with
/// "target lists can have at most 1664 entries".
const MAX_TARGET_LIST_ENTRIES: usize = 1664;

/// Route a possible simple-query COPY through the connection-owned producer.
///
/// A `COPY ... FROM STDIN` sent as a simple query is answered with
/// `CopyInResponse` and then PostgreSQL WAITS for copy data. The request was
/// encoded as one pre-built buffer, so this path has no channel to push
/// `CopyData` through and cannot finish the copy. Reporting
/// `Error::unexpected_message` and walking away is not enough: the session
/// stays in copy mode, the next frontend message is read as copy data, and
/// PostgreSQL answers `unexpected message type 0x50 during COPY from stdin`
/// followed by `FATAL: terminating connection because protocol synchronization
/// was lost`. The failure then lands on whatever ran NEXT - in a pool, the next
/// borrower, because nothing about the failing call marks the connection closed
/// or dirty.
///
/// `CopyFail` ends simple-query copy mode and PostgreSQL answers it with the
/// original query's `ErrorResponse + ReadyForQuery`. Extended protocol needs a
/// trailing `Sync`; simple protocol must not send one because it would earn a
/// second `ReadyForQuery` with no caller-visible request. A transaction pooler
/// can release the backend after the first terminator and discard the second,
/// stranding the driver's synthetic response slot.
///
/// The connection owns this producer so it writes no later request ahead of
/// the abort, still completes the abort if the caller drops its response
/// stream, and suppresses `CopyFail` when the query is rejected before COPY
/// mode or the conservative SQL classifier returns a false positive.
fn producerless_request(buf: Bytes, may_enter_copy_in: bool) -> RequestMessages {
    let initial = FrontendMessage::Raw(buf);
    if may_enter_copy_in {
        RequestMessages::CopyIn(CopyInReceiver::aborting_simple(
            initial,
            COPY_IN_UNSUPPORTED,
        ))
    } else {
        RequestMessages::Single(initial)
    }
}

/// Enqueue the batch and hand back its response stream, without awaiting it.
///
/// Split out of `batch_execute` so a caller can arm a cleanup guard around
/// exactly the window where one is owed: after the request reaches the
/// connection, and before its response has been consumed. Everything this
/// function does before `send` - logging, encoding - can fail or unwind while
/// the session is still untouched, and a guard armed across it would undo work
/// the caller never did.
pub(crate) fn start_batch_execute(client: &InnerClient, query: &str) -> Result<Responses, Error> {
    debug!("executing statement batch: {query}");

    let must_prequeue_copy_abort = may_enter_copy_in(query);
    let buf = encode(client, query)?;
    client.send(producerless_request(buf, must_prequeue_copy_abort))
}

pub(crate) fn start_batch_execute_with_error_cleanup(
    client: &InnerClient,
    query: &str,
    cleanup: &str,
) -> Result<Responses, Error> {
    debug!("executing statement batch: {query}");

    let must_prequeue_copy_abort = may_enter_copy_in(query);
    let query = encode(client, query)?;
    let cleanup = encode(client, cleanup)?;
    if !must_prequeue_copy_abort {
        return client
            .send_with_error_cleanup(FrontendMessage::Raw(query), FrontendMessage::Raw(cleanup));
    }

    let responses = client.send(producerless_request(query, true))?;
    drop(client.send_with(
        RequestMessages::Single(FrontendMessage::Raw(cleanup)),
        RequestDisposition::Housekeeping,
        TransactionEffect::MayChange,
    )?);
    Ok(responses)
}

/// Drain the response stream `start_batch_execute` returned.
pub(crate) async fn finish_batch_execute(mut responses: Responses) -> Result<(), Error> {
    let mut refused = None;
    loop {
        match responses.next().await? {
            Message::ReadyForQuery(_) => return refused.map_or(Ok(()), Err),
            Message::CommandComplete(_)
            | Message::EmptyQueryResponse
            | Message::RowDescription(_)
            | Message::DataRow(_) => {}
            // The connection-owned producer is already sending CopyFail. Keep
            // draining this stream to its server error.
            Message::CopyInResponse(_) => {}
            // A COPY OUT needs no abort - the server sends its own `CopyDone`
            // and finishes unaided - but it must not RETURN here either. A
            // batch can chain `COPY TO STDOUT; COPY FROM STDIN`, and returning
            // at the copy-out abandons the drain BEFORE the `CopyInResponse`
            // that does need aborting, leaving the server waiting for copy data
            // this API cannot send. That killed the connection outright.
            //
            // So: remember the refusal, keep draining, and report it at
            // `ReadyForQuery` if nothing worse arrives first. A later abort's
            // server-side error reaches the caller through `?` above and
            // outranks this, which is what makes the chained case report the
            // copy's own error rather than this one.
            Message::CopyOutResponse(_) | Message::CopyData(_) | Message::CopyDone => {
                refused.get_or_insert_with(Error::copy_out_unsupported);
            }
            _ => return Err(Error::unexpected_message()),
        }
    }
}

/// Drain the stream and report the command tag of the LAST statement in the
/// batch that completed.
///
/// The tag is the only place PostgreSQL says WHICH command it decided it had
/// run, and for one statement that answer is not the one that was sent:
/// `COMMIT` inside an aborted transaction block is executed, discards every
/// change, and completes with the tag `ROLLBACK` and no `ErrorResponse`.
/// [`finish_batch_execute`] drops tags, so a caller draining through it cannot
/// tell that outcome from a successful commit. libpq exposes it as
/// `PQcmdStatus`; this is the equivalent for the one caller that needs it.
///
/// `None` means the batch completed without any `CommandComplete` - an empty
/// query.
pub(crate) async fn finish_batch_execute_reporting_tag(
    mut responses: Responses,
) -> Result<Option<String>, Error> {
    let mut tag = None;
    let mut refused = None;
    loop {
        match responses.next().await? {
            Message::ReadyForQuery(_) => return refused.map_or(Ok(tag), Err),
            Message::CommandComplete(body) => {
                tag = Some(body.tag().map_err(Error::parse)?.to_string());
            }
            Message::EmptyQueryResponse | Message::RowDescription(_) | Message::DataRow(_) => {}
            Message::CopyInResponse(_) => {}
            // See the twin in `finish_batch_execute`: drain the copy-out rather
            // than returning at it, so a chained `COPY FROM STDIN` later in the
            // same batch is still reached and aborted.
            Message::CopyOutResponse(_) | Message::CopyData(_) | Message::CopyDone => {
                refused.get_or_insert_with(Error::copy_out_unsupported);
            }
            _ => return Err(Error::unexpected_message()),
        }
    }
}

fn encode(client: &InnerClient, query: &str) -> Result<Bytes, Error> {
    client.with_buf(|buf| {
        frontend::query(query, buf).map_err(Error::encode)?;
        Ok(buf.split().freeze())
    })
}

/// Whether a simple Query message can put PostgreSQL into COPY-IN mode.
///
/// Recovery must be selective: PostgreSQL does not describe all statements in
/// a simple Query ahead of execution, so the original SQL is the only way to
/// decide whether the request needs the connection-owned COPY producer. An
/// ordinary query must remain a normal request; a possible `COPY ... FROM
/// STDIN` must keep later writes behind its eventual `CopyFail` even if the
/// caller drops the response stream.
///
/// This lexer recognises statement-leading COPY and a top-level FROM STDIN
/// source while excluding strings, quoted identifiers, dollar-quoted bodies,
/// comments, and the FROM inside `COPY (SELECT ...) TO STDOUT`.
pub(crate) fn may_enter_copy_in(query: &str) -> bool {
    // `standard_conforming_strings` is session state and PostgreSQL reports it,
    // but this low-level helper only has the SQL. Accept a COPY found under
    // either interpretation so a backslash-escaped quote can never hide a real
    // statement. A false positive is safe: the connection observes the
    // request's ReadyForQuery and suppresses CopyFail before polling it.
    may_enter_copy_in_with_string_mode(query, false)
        || may_enter_copy_in_with_string_mode(query, true)
}

fn may_enter_copy_in_with_string_mode(query: &str, ordinary_backslash_escapes: bool) -> bool {
    let bytes = query.as_bytes();
    let mut index = 0;
    let mut statement_start = true;
    let mut copy = false;
    let mut copy_from = false;
    let mut paren_depth = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_whitespace() || byte == b'\x0b' => index += 1,
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut depth = 1usize;
                while index < bytes.len() && depth != 0 {
                    if bytes[index..].starts_with(b"/*") {
                        depth += 1;
                        index += 2;
                    } else if bytes[index..].starts_with(b"*/") {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            }
            b'\'' => {
                copy_from = false;
                let escape_string = ordinary_backslash_escapes
                    || (index > 0
                        && matches!(bytes[index - 1], b'e' | b'E')
                        && (index == 1
                            || !matches!(
                                bytes[index - 2],
                                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$'
                            )));
                index += 1;
                while index < bytes.len() {
                    if escape_string && bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == b'\'' {
                        index += 1;
                        if bytes.get(index) == Some(&b'\'') {
                            index += 1;
                        } else {
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'"' => {
                copy_from = false;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'"' {
                        index += 1;
                        // NO TEST CAN BIND THIS ESCAPE ARM, and that is a
                        // property of the function rather than a gap. Deleting
                        // it does not expose any byte: the first quote closes
                        // this identifier and the second immediately opens the
                        // next, so the same span stays inside quotes and the
                        // COPY verdict is unchanged. Checked exhaustively over
                        // every string of length <= 12 in {'"', 'x', ';'} -
                        // 797,161 inputs, zero disagreements. It earns its
                        // place by keeping identifier boundaries honest for a
                        // future caller that wants them, not by changing what
                        // THIS function returns, so a mutation report calling
                        // it unbound is right and no test should be invented
                        // to satisfy it.
                        if bytes.get(index) == Some(&b'"') {
                            index += 1;
                        } else {
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'$' => {
                let tag_start = index;
                let mut tag_end = index + 1;
                if bytes.get(tag_end).is_some_and(|byte| {
                    byte.is_ascii_alphabetic() || *byte == b'_' || !byte.is_ascii()
                }) {
                    tag_end += 1;
                    while bytes.get(tag_end).is_some_and(|byte| {
                        byte.is_ascii_alphanumeric() || *byte == b'_' || !byte.is_ascii()
                    }) {
                        tag_end += 1;
                    }
                }
                if bytes.get(tag_end) != Some(&b'$') {
                    copy_from = false;
                    index += 1;
                    continue;
                }

                copy_from = false;
                let delimiter = &bytes[tag_start..=tag_end];
                index = tag_end + 1;
                while index + delimiter.len() <= bytes.len()
                    && &bytes[index..index + delimiter.len()] != delimiter
                {
                    index += 1;
                }
                index = (index + delimiter.len()).min(bytes.len());
            }
            b';' if paren_depth == 0 => {
                statement_start = true;
                copy = false;
                copy_from = false;
                index += 1;
            }
            b'(' => {
                paren_depth += 1;
                copy_from = false;
                index += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                copy_from = false;
                index += 1;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || !byte.is_ascii() => {
                let start = index;
                index += 1;
                while bytes.get(index).is_some_and(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'$') || !byte.is_ascii()
                }) {
                    index += 1;
                }
                let word = &bytes[start..index];
                if statement_start {
                    statement_start = false;
                    copy = word.eq_ignore_ascii_case(b"copy");
                    copy_from = false;
                } else if copy && paren_depth == 0 {
                    if copy_from
                        && (word.eq_ignore_ascii_case(b"stdin")
                            || word.eq_ignore_ascii_case(b"stdout"))
                    {
                        return true;
                    }
                    copy_from = word.eq_ignore_ascii_case(b"from");
                }
            }
            _ => {
                copy_from = false;
                index += 1;
            }
        }
    }

    false
}

pin_project! {
    /// A stream of simple query results.
    #[project(!Unpin)]
    pub struct SimpleQueryStream {
        responses: Responses,
        columns: Option<Arc<[SimpleColumn]>>,
        // A `COPY TO STDOUT` this path cannot deliver is DRAINED rather than
        // returned at, so a chained `COPY FROM STDIN` later in the same batch
        // is still reached and aborted; the refusal is reported at
        // `ReadyForQuery`. See the twin in `finish_batch_execute`.
        copy_out_refused: bool,
    }
}

impl Stream for SimpleQueryStream {
    type Item = Result<SimpleQueryMessage, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        // A loop, not a single match: the copy-mode arm produces no item of its
        // own, and returning `Pending` after it would park a stream nobody will
        // wake.
        loop {
            match ready!(this.responses.poll_next(cx)?) {
                Message::CommandComplete(body) => {
                    let rows = extract_row_affected(&body)?;
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::CommandComplete(rows))));
                }
                Message::EmptyQueryResponse => {
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::CommandComplete(0))));
                }
                Message::RowDescription(body) => {
                    // Reserve for a row PostgreSQL could actually describe, not
                    // for the count the peer declares. `Fields::size_hint`
                    // returns that u16 verbatim (postgres-protocol 0.6.12,
                    // backend.rs), and `collect` reserves it, so
                    // `T 00 00 00 06 ff ff` reserved 65535 `SimpleColumn` slots
                    // - about 1.6 MB - before failing on the absent first
                    // field.
                    //
                    // Unlike the DataRow sites in `row.rs` there is no buffer
                    // length to bound this with: `RowDescriptionBody` exposes
                    // only `fields()`. The server's own ceiling is the bound
                    // instead. MEASURED on 16.14, 2026-08-26: a SELECT of 1664
                    // columns succeeds and 1665 fails with "target lists can
                    // have at most 1664 entries", so no well-formed
                    // RowDescription exceeds it.
                    //
                    // This clamps the RESERVATION only, and never rejects: a
                    // peer that really sends more just makes the Vec grow.
                    let mut fields = body.fields();
                    let mut collected: Vec<SimpleColumn> =
                        Vec::with_capacity(fields.size_hint().0.min(MAX_TARGET_LIST_ENTRIES));
                    while let Some(field) = fields.next().map_err(Error::parse)? {
                        collected.push(SimpleColumn::new(
                            field.name().to_string(),
                            field.type_oid(),
                            SimpleQueryFormat::from_code(field.format())?,
                        ));
                    }
                    let columns: Arc<[SimpleColumn]> = collected.into();

                    *this.columns = Some(columns.clone());
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::RowDescription(columns))));
                }
                Message::DataRow(body) => {
                    let row = match &this.columns {
                        Some(columns) => SimpleQueryRow::new(columns.clone(), body)?,
                        None => return Poll::Ready(Some(Err(Error::unexpected_message()))),
                    };
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::Row(row))));
                }
                Message::ReadyForQuery(_) => {
                    // The refusal is emitted here, once, so the stream reports
                    // it exactly like the old `return` did while still having
                    // drained to a known frame boundary.
                    if *this.copy_out_refused {
                        *this.copy_out_refused = false;
                        return Poll::Ready(Some(Err(Error::copy_out_unsupported())));
                    }
                    return Poll::Ready(None);
                }
                Message::CopyOutResponse(_) | Message::CopyData(_) | Message::CopyDone => {
                    *this.copy_out_refused = true;
                }
                // The connection-owned producer is already sending CopyFail.
                Message::CopyInResponse(_) => {}
                _ => return Poll::Ready(Some(Err(Error::unexpected_message()))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::may_enter_copy_in;

    #[test]
    fn copy_in_classifier_finds_frontend_copy_sources() {
        for query in [
            "COPY t FROM STDIN",
            "COPY t FROM STDOUT",
            "SELECT 1; COPY t (a) FrOm /* nested /* comment */ ok */ StDiN",
            "; -- lead\n COPY BINARY t FROM STDIN WITH (FORMAT binary)",
            r"SELECT 'a\'; COPY t FROM STDIN",
            r"SELECT 'x\'; SELECT 2'; COPY t FROM STDIN",
            "SELECT $é$'$é$; COPY t FROM STDIN",
        ] {
            assert!(may_enter_copy_in(query), "missed COPY-IN query: {query}");
        }
        assert!(
            may_enter_copy_in("COPY t FROM\x0bSTDIN"),
            "missed COPY-IN query with PostgreSQL vertical-tab whitespace"
        );
    }

    #[test]
    fn copy_in_classifier_scans_past_doubled_quoted_identifier_delimiters() {
        assert!(may_enter_copy_in(
            r#"SELECT "not "" copy"; COPY t FROM STDIN"#
        ));
    }

    #[test]
    fn copy_in_classifier_ignores_non_commands_and_copy_out() {
        for query in [
            "SELECT 'COPY t FROM STDIN'",
            r"SELECT E'not COPY t FROM STDIN \' either'",
            "SELECT $$ COPY t FROM STDIN $$",
            "SELECT $tag$ COPY t FROM STDIN $tag$",
            "SELECT \"COPY\", \"FROM\", \"STDIN\"",
            "COPY t TO STDOUT",
            "COPY (SELECT * FROM stdin) TO STDOUT",
            "-- COPY t FROM STDIN\nSELECT 1",
            "/* COPY t FROM STDIN */ SELECT 1",
        ] {
            assert!(!may_enter_copy_in(query), "misclassified query: {query}");
        }
    }
}
