// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::client::InnerClient;
use crate::codec::FrontendMessage;
use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
use crate::types::Type;
use postgres_protocol::message::frontend;
use std::sync::{Arc, Weak};

struct StatementInner {
    client: Weak<InnerClient>,
    name: String,
    params: Vec<Type>,
    columns: Vec<Column>,
}

/// Queues `Close S` + `Sync` for a server-side prepared statement.
///
/// Fire-and-forget: the response stream is dropped straight away, so the
/// DEALLOCATE lands on the connection task's own schedule. Closing a name the
/// server does not hold is a no-op there, which is what lets a caller fire this
/// for a statement whose `Parse` may never have succeeded.
pub(crate) fn close_statement(client: &InnerClient, name: &str) {
    let buf = client.with_buf(|buf| {
        if let Err(e) = frontend::close(b'S', name, buf) {
            // The name cannot be encoded (interior NUL, or a length overflow),
            // so there is nothing to send and the statement stays on the server
            // until the session ends. Report that rather than drop the cause:
            // silence here is indistinguishable from a successful DEALLOCATE.
            log::error!(
                "compio-postgres: cannot encode Close for prepared statement {name}: {e}"
            );
            return None;
        }
        frontend::sync(buf);
        Some(buf.split().freeze())
    });
    if let Some(buf) = buf {
        let _ = client.send_with(
            RequestMessages::Single(FrontendMessage::Raw(buf)),
            RequestDisposition::Housekeeping,
            TransactionEffect::Neutral,
        );
    }
}

impl Drop for StatementInner {
    fn drop(&mut self) {
        if self.name.is_empty() {
            // Unnamed statements don't need to be closed
            return;
        }
        if let Some(client) = self.client.upgrade() {
            close_statement(&client, &self.name);
        }
    }
}

/// A prepared statement.
///
/// Prepared statements can only be used with the connection that created them.
#[derive(Clone)]
pub struct Statement(Arc<StatementInner>);

impl Statement {
    pub(crate) fn new(
        inner: &Arc<InnerClient>,
        name: String,
        params: Vec<Type>,
        columns: Vec<Column>,
    ) -> Statement {
        Statement(Arc::new(StatementInner {
            client: Arc::downgrade(inner),
            name,
            params,
            columns,
        }))
    }

    pub(crate) fn unnamed(params: Vec<Type>, columns: Vec<Column>) -> Statement {
        Statement(Arc::new(StatementInner {
            client: Weak::new(),
            name: String::new(),
            params,
            columns,
        }))
    }

    pub(crate) fn name(&self) -> &str {
        &self.0.name
    }

    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn invalidate_cache_on_error(&self, error: &crate::Error) {
        if let Some(client) = self.0.client.upgrade() {
            client.invalidate_cached_statement_on_error(self, error);
        }
    }

    /// Returns the expected types of the statement's parameters.
    pub fn params(&self) -> &[Type] {
        &self.0.params
    }

    /// Returns information about the columns returned when the statement is queried.
    pub fn columns(&self) -> &[Column] {
        &self.0.columns
    }
}

impl std::fmt::Debug for Statement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("Statement")
            .field("name", &self.0.name)
            .field("params", &self.0.params)
            .field("columns", &self.0.columns)
            .finish_non_exhaustive()
    }
}

/// Information about a column of a query.
#[derive(Debug)]
pub struct Column {
    pub(crate) name: String,
    pub(crate) table_oid: Option<u32>,
    pub(crate) column_id: Option<i16>,
    pub(crate) type_modifier: i32,
    pub(crate) r#type: Type,
}

impl Column {
    /// Returns the name of the column.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the OID of the underlying database table.
    pub fn table_oid(&self) -> Option<u32> {
        self.table_oid
    }

    /// Return the column ID within the underlying database table.
    pub fn column_id(&self) -> Option<i16> {
        self.column_id
    }

    /// Return the type modifier
    pub fn type_modifier(&self) -> i32 {
        self.type_modifier
    }

    /// Returns the type of the column.
    pub fn type_(&self) -> &Type {
        &self.r#type
    }
}
