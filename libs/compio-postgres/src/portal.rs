// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::Statement;
use crate::client::InnerClient;
use crate::codec::FrontendMessage;
use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
use parking_lot::Mutex;
use postgres_protocol::message::frontend;
use std::io;
use std::sync::{Arc, Weak};

struct Inner {
    client: Weak<InnerClient>,
    name: String,
    statement: Statement,
    scope: PortalScope,
}

#[derive(Clone)]
pub(crate) struct PortalScope(Arc<Mutex<PortalScopeState>>);

enum PortalScopeState {
    Active,
    Reparented(PortalScope),
    Invalid,
}

impl PortalScope {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(PortalScopeState::Active)))
    }

    pub(crate) fn invalidate(&self) {
        *self.0.lock() = PortalScopeState::Invalid;
    }

    pub(crate) fn reparent(&self, parent: PortalScope) {
        *self.0.lock() = PortalScopeState::Reparented(parent);
    }

    /// Run while every scope from this portal to its active ancestor is locked.
    /// A transaction end therefore cannot race between validating a name and
    /// enqueueing the Execute or Close which uses it.
    fn with_active<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let state = self.0.lock();
        match &*state {
            PortalScopeState::Active => Some(f()),
            PortalScopeState::Reparented(parent) => parent.with_active(f),
            PortalScopeState::Invalid => None,
        }
    }
}

pub(crate) fn close_portal(client: &InnerClient, name: &str) {
    let buf = client.with_buf(|buf| {
        if frontend::close(b'P', name, buf).is_err() {
            // Generated names cannot contain NUL or overflow, but Drop cannot
            // report an encoder failure if that invariant is ever broken.
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

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(client) = self.client.upgrade() {
            self.scope.with_active(|| close_portal(&client, &self.name));
        }
    }
}

/// A portal.
///
/// Portals can only be used with the connection that created them, and only exist for the duration of the transaction
/// in which they were created.
#[derive(Clone)]
pub struct Portal(Arc<Inner>);

impl std::fmt::Debug for Portal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Portal")
            .field("name", &self.0.name)
            .field("statement_name", &self.0.statement.name())
            .finish_non_exhaustive()
    }
}

impl Portal {
    pub(crate) fn new(
        client: &Arc<InnerClient>,
        name: String,
        statement: Statement,
        scope: PortalScope,
    ) -> Portal {
        Portal(Arc::new(Inner {
            client: Arc::downgrade(client),
            name,
            statement,
            scope,
        }))
    }

    pub(crate) fn with_live_on<T>(
        &self,
        client: &Arc<InnerClient>,
        f: impl FnOnce() -> Result<T, crate::Error>,
    ) -> Result<T, crate::Error> {
        let same_client = self
            .0
            .client
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, client));
        if !same_client {
            return Err(invalid_portal());
        }

        self.0
            .scope
            .with_active(f)
            .unwrap_or_else(|| Err(invalid_portal()))
    }

    pub(crate) fn name(&self) -> &str {
        &self.0.name
    }

    pub(crate) fn statement(&self) -> &Statement {
        &self.0.statement
    }
}

fn invalid_portal() -> crate::Error {
    crate::Error::encode(io::Error::new(
        io::ErrorKind::InvalidInput,
        "portal no longer belongs to an active transaction on this client",
    ))
}
