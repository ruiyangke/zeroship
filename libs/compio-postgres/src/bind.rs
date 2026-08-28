// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::client::InnerClient;
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::portal::{self, PortalScope};
use crate::types::BorrowToSql;
use crate::{Column, Error, Portal, Statement, prepare, query};
use fallible_iterator::FallibleIterator;
use futures_channel::oneshot;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// Own a portal after `BindComplete` and until the `Portal` is built.
///
/// A rejected Bind never owns its requested name, so arming this before
/// `BindComplete` would let an error path close the pre-existing portal whose
/// name caused the rejection.
struct PortalCleanup {
    client: Weak<InnerClient>,
    name: Option<String>,
}

impl PortalCleanup {
    fn new(client: &Arc<InnerClient>, name: &str) -> Self {
        Self {
            client: Arc::downgrade(client),
            name: Some(name.to_string()),
        }
    }

    fn disarm(mut self) -> String {
        self.name.take().expect("the portal name is still owned")
    }
}

impl Drop for PortalCleanup {
    fn drop(&mut self) {
        let (Some(client), Some(name)) = (self.client.upgrade(), self.name.take()) else {
            return;
        };
        portal::close_portal(&client, &name);
    }
}

pub async fn bind<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
    unnamed_sql: Option<&str>,
    scope: PortalScope,
) -> Result<Portal, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let name = format!("p{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let unnamed = unnamed_sql.is_some();
    let buf = client.with_buf(|buf| {
        if let Some(sql) = unnamed_sql {
            frontend::parse(
                "",
                sql,
                statement.params().iter().map(crate::types::Type::oid),
                buf,
            )
            .map_err(Error::encode)?;
        }
        query::encode_bind(&statement, params, &name, buf)?;
        if unnamed_sql.is_some() {
            frontend::describe(b'P', &name, buf).map_err(Error::encode)?;
        }
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let mut responses = client.send_statement(
        RequestMessages::Single(FrontendMessage::Raw(buf)),
        &statement,
    )?;
    let client = Arc::clone(client);
    let (sender, receiver) = oneshot::channel();
    compio::runtime::spawn(async move {
        let result = finish_bind(client, statement, &mut responses, name, unnamed, scope).await;
        // If the caller abandoned bind(), dropping an awarded Portal performs
        // the Close(P). A rejection carries no Portal and therefore closes
        // nothing.
        let _ = sender.send(result);
    })
    .detach();

    receiver.await.unwrap_or_else(|_| Err(Error::closed()))
}

async fn finish_bind(
    client: Arc<InnerClient>,
    statement: Statement,
    responses: &mut crate::client::Responses,
    name: String,
    unnamed: bool,
    scope: PortalScope,
) -> Result<Portal, Error> {
    if unnamed {
        let message = match responses.next().await {
            Ok(message) => message,
            Err(error) => {
                statement.invalidate_cache_on_error(&error);
                return Err(error);
            }
        };
        if !matches!(message, Message::ParseComplete) {
            return Err(Error::unexpected_message());
        }
    }

    let message = match responses.next().await {
        Ok(message) => message,
        Err(error) => {
            statement.invalidate_cache_on_error(&error);
            return Err(error);
        }
    };
    match message {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
    }
    let cleanup = PortalCleanup::new(&client, &name);

    let row_description = if unnamed {
        match responses.next().await? {
            Message::RowDescription(body) => Some(body),
            Message::NoData => None,
            _ => return Err(Error::unexpected_message()),
        }
    } else {
        None
    };

    // BindComplete (and the optional descriptor) only confirms the prefix of
    // this Sync-terminated exchange. Keep the response alive through
    // ReadyForQuery so a later ErrorResponse owns the bind result, and do so
    // before decoding the descriptor can produce a competing local error.
    match responses.next().await? {
        Message::ReadyForQuery(_) => {}
        _ => return Err(Error::unexpected_message()),
    }

    let statement = if unnamed {
        let mut columns = Vec::new();
        if let Some(row_description) = row_description {
            let mut fields = row_description.fields();
            while let Some(field) = fields.next().map_err(Error::parse)? {
                columns.push(Column {
                    name: field.name().to_string(),
                    table_oid: Some(field.table_oid()).filter(|oid| *oid != 0),
                    column_id: Some(field.column_id()).filter(|id| *id != 0),
                    type_modifier: field.type_modifier(),
                    r#type: prepare::get_type(&client, field.type_oid()).await?,
                });
            }
        }
        Statement::unnamed_with_copy_in(
            statement.params().to_vec(),
            columns,
            statement.may_enter_copy_in(),
        )
    } else {
        statement
    };

    let name = cleanup.disarm();
    Ok(Portal::new(&client, name, statement, scope))
}

#[cfg(test)]
mod tests {
    use super::PortalCleanup;
    use crate::client::{Client, ResponseMessages, StatementCacheSettings};
    use crate::codec::{BackendMessages, FrontendMessage};
    use crate::config::{ProtocolVersion, SslMode, SslNegotiation};
    use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
    use crate::{Error, Statement};
    use bytes::BytesMut;
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(body.len() + 5);
        frame.push(tag);
        frame.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 4).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    async fn bind_with_terminal_response(unnamed: bool) -> Result<crate::Portal, Error> {
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
        let statement =
            Statement::new(&inner, "s_terminal_bind".to_string(), vec![], vec![], false);
        let bind = super::bind(
            &inner,
            statement,
            std::iter::empty::<i32>(),
            unnamed.then_some("SELECT 1"),
            crate::portal::PortalScope::new(),
        );
        let respond = async {
            let mut request = receiver
                .next()
                .await
                .expect("bind did not enqueue its protocol request");
            let mut bytes = BytesMut::new();
            if unnamed {
                bytes.extend_from_slice(&backend_frame(b'1', b""));
            }
            bytes.extend_from_slice(&backend_frame(b'2', b""));
            if unnamed {
                bytes.extend_from_slice(&backend_frame(b'n', b""));
            }
            bytes.extend_from_slice(&backend_frame(
                b'E',
                b"SFATAL\0VFATAL\0C57P01\0Mscripted termination after Bind\0\0",
            ));
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    bytes,
                )))
                .expect("deliver the terminal bind response");
        };

        let (result, ()) = futures_util::join!(bind, respond);
        result
    }

    #[compio::test]
    async fn named_bind_preserves_a_terminal_error_after_bind_complete() {
        let error = match bind_with_terminal_response(false).await {
            Err(error) => error,
            Ok(_) => panic!("named bind returned success before its terminal SQLSTATE 57P01"),
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "named bind discarded terminal SQLSTATE 57P01: {error}"
        );
    }

    #[compio::test]
    async fn unnamed_bind_preserves_a_terminal_error_after_description() {
        let error = match bind_with_terminal_response(true).await {
            Err(error) => error,
            Ok(_) => panic!("unnamed bind returned success before its terminal SQLSTATE 57P01"),
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "unnamed bind discarded terminal SQLSTATE 57P01: {error}"
        );
    }

    #[test]
    fn dropping_armed_portal_cleanup_enqueues_close() {
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

        const NAME: &str = "p_cancelled_bind";
        drop(PortalCleanup::new(client.inner(), NAME));

        let request = receiver
            .try_recv()
            .expect("dropping an armed cleanup must enqueue portal Close");
        assert_eq!(request.disposition, RequestDisposition::Housekeeping);
        assert_eq!(request.transaction_effect, TransactionEffect::Neutral);
        let RequestMessages::Single(FrontendMessage::Raw(bytes)) = request.messages else {
            panic!("portal cleanup did not enqueue one raw protocol batch");
        };
        assert_eq!(bytes[0], b'C');
        assert_eq!(bytes[5], b'P');
        assert_eq!(&bytes[6..6 + NAME.len()], NAME.as_bytes());
        assert_eq!(&bytes[7 + NAME.len()..], b"S\0\0\0\x04");
    }
}
