// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::client::InnerClient;
use crate::codec::FrontendMessage;
use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
use crate::types::BorrowToSql;
use crate::{Column, Error, Portal, Statement, prepare, query};
use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// Own a portal after its bind is enqueued and until the `Portal` is built.
/// Without this guard, cancellation in any response await would leave a named
/// portal alive until the surrounding transaction ended.
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
        let buf = client.with_buf(|buf| -> Result<_, Error> {
            frontend::close(b'P', &name, buf).map_err(Error::encode)?;
            frontend::sync(buf);
            Ok(buf.split().freeze())
        });
        if let Ok(buf) = buf {
            let _ = client.send_with(
                RequestMessages::Single(FrontendMessage::Raw(buf)),
                RequestDisposition::Housekeeping,
                TransactionEffect::Neutral,
            );
        }
    }
}

pub async fn bind<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
    unnamed_sql: Option<&str>,
) -> Result<Portal, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let name = format!("p{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
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
    let cleanup = PortalCleanup::new(client, &name);

    if unnamed_sql.is_some() {
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

    let statement = if unnamed_sql.is_some() {
        let row_description = match responses.next().await? {
            Message::RowDescription(body) => Some(body),
            Message::NoData => None,
            _ => return Err(Error::unexpected_message()),
        };
        let mut columns = Vec::new();
        if let Some(row_description) = row_description {
            let mut fields = row_description.fields();
            while let Some(field) = fields.next().map_err(Error::parse)? {
                columns.push(Column {
                    name: field.name().to_string(),
                    table_oid: Some(field.table_oid()).filter(|oid| *oid != 0),
                    column_id: Some(field.column_id()).filter(|id| *id != 0),
                    type_modifier: field.type_modifier(),
                    r#type: prepare::get_type(client, field.type_oid()).await?,
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
    Ok(Portal::new(client, name, statement))
}

#[cfg(test)]
mod tests {
    use super::PortalCleanup;
    use crate::client::{Client, StatementCacheSettings};
    use crate::codec::FrontendMessage;
    use crate::config::{ProtocolVersion, SslMode, SslNegotiation};
    use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
    use futures_channel::mpsc;
    use std::num::NonZeroUsize;

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
