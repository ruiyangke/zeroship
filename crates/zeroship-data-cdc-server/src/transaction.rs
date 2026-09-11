//! Commit boundary for non-streamed pgoutput transactions.
//!
//! Relation metadata is consumed internally as it arrives. Row changes leave
//! this buffer only with their commit position. Buffer exhaustion discards the
//! pending changes and marks the committed batch for subscriber resynchronization.

use compio_postgres::replication::pgoutput::{PgOutputMessage, TupleColumn, TupleData};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProtocolError {
    NestedBegin,
    OutsideTransaction,
    InvalidCommit,
    UnsupportedProtocol,
}

#[derive(Debug)]
pub(crate) struct CommittedBatch {
    pub end_lsn: u64,
    pub changes: Vec<PgOutputMessage>,
    pub needs_resync: bool,
}

#[derive(Debug)]
pub(crate) enum Action {
    Pending,
    Metadata(PgOutputMessage),
    Commit(CommittedBatch),
}

#[derive(Debug)]
struct PendingTransaction {
    commit_lsn: u64,
    bytes: usize,
    changes: Vec<PgOutputMessage>,
    needs_resync: bool,
}

#[derive(Debug)]
pub(crate) struct TransactionBuffer {
    max_bytes: usize,
    max_changes: usize,
    pending: Option<PendingTransaction>,
}

impl TransactionBuffer {
    pub fn new(max_bytes: usize, max_changes: usize) -> Self {
        Self {
            max_bytes,
            max_changes,
            pending: None,
        }
    }

    pub fn push(&mut self, message: PgOutputMessage) -> Result<Action, ProtocolError> {
        use PgOutputMessage as Message;
        match message {
            Message::Begin { final_lsn, .. } => {
                if self.pending.is_some() {
                    return Err(ProtocolError::NestedBegin);
                }
                self.pending = Some(PendingTransaction {
                    commit_lsn: final_lsn,
                    bytes: 0,
                    changes: Vec::new(),
                    needs_resync: false,
                });
                Ok(Action::Pending)
            }
            Message::Commit {
                flags,
                commit_lsn,
                end_lsn,
                ..
            } => {
                let pending = self
                    .pending
                    .take()
                    .ok_or(ProtocolError::OutsideTransaction)?;
                if flags != 0 || commit_lsn != pending.commit_lsn || end_lsn <= commit_lsn {
                    return Err(ProtocolError::InvalidCommit);
                }
                Ok(Action::Commit(CommittedBatch {
                    end_lsn,
                    changes: pending.changes,
                    needs_resync: pending.needs_resync,
                }))
            }
            Message::Relation { xid: None, .. } => Ok(Action::Metadata(message)),
            Message::Type { xid: None, .. }
            | Message::Origin { .. }
            | Message::Message { xid: None, .. } => Ok(Action::Pending),
            Message::Insert { xid: None, .. }
            | Message::Update { xid: None, .. }
            | Message::Delete { xid: None, .. }
            | Message::Truncate { xid: None, .. } => {
                let pending = self
                    .pending
                    .as_mut()
                    .ok_or(ProtocolError::OutsideTransaction)?;
                if pending.needs_resync {
                    return Ok(Action::Pending);
                }
                pending.bytes = pending.bytes.saturating_add(retained_bytes(&message));
                if pending.bytes > self.max_bytes || pending.changes.len() >= self.max_changes {
                    pending.changes = Vec::new();
                    pending.needs_resync = true;
                } else {
                    pending.changes.push(message);
                }
                Ok(Action::Pending)
            }
            _ => Err(ProtocolError::UnsupportedProtocol),
        }
    }
}

fn tuple_bytes(tuple: &TupleData) -> usize {
    tuple.columns.iter().fold(
        tuple
            .columns
            .capacity()
            .saturating_mul(std::mem::size_of::<TupleColumn>()),
        |total, column| {
            total.saturating_add(match column {
                TupleColumn::Text(value) => value.capacity(),
                TupleColumn::Binary(value) => value.len(),
                TupleColumn::Null | TupleColumn::Toasted => 0,
            })
        },
    )
}

fn retained_bytes(message: &PgOutputMessage) -> usize {
    let tuple_size = match message {
        PgOutputMessage::Insert { new_tuple, .. } => tuple_bytes(new_tuple),
        PgOutputMessage::Update {
            new_tuple,
            old_tuple,
            ..
        } => tuple_bytes(new_tuple)
            .saturating_add(old_tuple.as_ref().map_or(0, |old| tuple_bytes(old.tuple()))),
        PgOutputMessage::Delete { old_tuple, .. } => tuple_bytes(old_tuple.tuple()),
        PgOutputMessage::Truncate { relation_ids, .. } => relation_ids
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>()),
        _ => 0,
    };
    // Account for spare capacity when the pending message vector grows.
    tuple_size.saturating_add(std::mem::size_of::<PgOutputMessage>().saturating_mul(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use compio_postgres::replication::pgoutput::OldTuple;

    fn begin() -> PgOutputMessage {
        PgOutputMessage::Begin {
            final_lsn: 100,
            commit_timestamp: 0,
            xid: 1,
        }
    }

    fn commit() -> PgOutputMessage {
        PgOutputMessage::Commit {
            flags: 0,
            commit_lsn: 100,
            end_lsn: 110,
            commit_timestamp: 0,
        }
    }

    fn insert(value: &str) -> PgOutputMessage {
        PgOutputMessage::Insert {
            xid: None,
            rel_id: 1,
            new_tuple: TupleData {
                columns: vec![TupleColumn::Text(value.to_owned())],
            },
        }
    }

    #[test]
    fn changes_leave_only_with_the_commit_position() {
        let mut buffer = TransactionBuffer::new(4096, 10);
        assert!(matches!(buffer.push(begin()).unwrap(), Action::Pending));
        assert!(matches!(
            buffer.push(insert("secret")).unwrap(),
            Action::Pending
        ));
        let Action::Commit(batch) = buffer.push(commit()).unwrap() else {
            panic!("commit")
        };
        assert_eq!(batch.end_lsn, 110);
        assert_eq!(batch.changes, vec![insert("secret")]);
        assert!(!batch.needs_resync);
    }

    #[test]
    fn each_buffer_limit_discards_partial_delivery_and_recovers_after_commit() {
        for (max_bytes, max_changes) in [(1, 10), (4096, 1)] {
            let mut buffer = TransactionBuffer::new(max_bytes, max_changes);
            buffer.push(begin()).unwrap();
            buffer.push(insert("first")).unwrap();
            buffer.push(insert("second")).unwrap();
            buffer.push(insert("third")).unwrap();
            let Action::Commit(batch) = buffer.push(commit()).unwrap() else {
                panic!("commit")
            };
            assert!(batch.needs_resync);
            assert!(batch.changes.is_empty());
            buffer.push(begin()).unwrap();
            let Action::Commit(batch) = buffer.push(commit()).unwrap() else {
                panic!("commit")
            };
            assert!(!batch.needs_resync);
        }
    }

    #[test]
    fn invalid_transaction_order_and_protocol_are_refused() {
        let mut buffer = TransactionBuffer::new(4096, 10);
        assert_eq!(
            buffer.push(insert("row")).unwrap_err(),
            ProtocolError::OutsideTransaction
        );
        assert_eq!(
            buffer.push(commit()).unwrap_err(),
            ProtocolError::OutsideTransaction
        );
        buffer.push(begin()).unwrap();
        assert_eq!(
            buffer.push(begin()).unwrap_err(),
            ProtocolError::NestedBegin
        );
        assert_eq!(
            buffer.push(PgOutputMessage::StreamStop).unwrap_err(),
            ProtocolError::UnsupportedProtocol
        );
        let mut wrong_commit = commit();
        if let PgOutputMessage::Commit { commit_lsn, .. } = &mut wrong_commit {
            *commit_lsn = 99;
        }
        assert_eq!(
            buffer.push(wrong_commit).unwrap_err(),
            ProtocolError::InvalidCommit
        );
    }

    #[test]
    fn old_row_image_and_truncate_survive_the_commit_boundary() {
        let mut buffer = TransactionBuffer::new(4096, 10);
        let old = OldTuple::Key(TupleData {
            columns: vec![TupleColumn::Text("old-id".into())],
        });
        let delete = PgOutputMessage::Delete {
            xid: None,
            rel_id: 1,
            old_tuple: old,
        };
        let truncate = PgOutputMessage::Truncate {
            xid: None,
            options: 0,
            relation_ids: vec![1],
        };
        buffer.push(begin()).unwrap();
        buffer.push(delete.clone()).unwrap();
        buffer.push(truncate.clone()).unwrap();
        let Action::Commit(batch) = buffer.push(commit()).unwrap() else {
            panic!("commit")
        };
        assert_eq!(batch.changes, vec![delete, truncate]);
    }
}
