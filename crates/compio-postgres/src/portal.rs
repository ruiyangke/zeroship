// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::Statement;
use crate::client::InnerClient;
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use postgres_protocol::message::frontend;
use std::sync::{Arc, Weak};

struct Inner {
    client: Weak<InnerClient>,
    name: String,
    statement: Statement,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(client) = self.client.upgrade() {
            let buf = client.with_buf(|buf| {
                if frontend::close(b'P', &self.name, buf).is_err() {
                    // can't encode name (NUL or overflow); silently give up CLOSE
                    return None;
                }
                frontend::sync(buf);
                Some(buf.split().freeze())
            });
            if let Some(buf) = buf {
                let _ = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)));
            }
        }
    }
}

/// A portal.
///
/// Portals can only be used with the connection that created them, and only exist for the duration of the transaction
/// in which they were created.
#[derive(Clone)]
pub struct Portal(Arc<Inner>);

impl Portal {
    pub(crate) fn new(client: &Arc<InnerClient>, name: String, statement: Statement) -> Portal {
        Portal(Arc::new(Inner {
            client: Arc::downgrade(client),
            name,
            statement,
        }))
    }

    pub(crate) fn name(&self) -> &str {
        &self.0.name
    }

    pub(crate) fn statement(&self) -> &Statement {
        &self.0.statement
    }
}
