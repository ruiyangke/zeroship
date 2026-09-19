//! A settlement result belongs to an admitted transaction, even after its lane
//! is released and another transaction claims the same app.

use std::{cell::RefCell, rc::Rc};

use futures::{FutureExt, channel::oneshot, future::Shared};

use super::{Driven, TxProtocolError};

#[derive(Clone, Debug)]
pub(crate) struct Completion {
    sender: Rc<RefCell<Option<oneshot::Sender<Driven>>>>,
    receiver: Shared<oneshot::Receiver<Driven>>,
}

impl Default for Completion {
    fn default() -> Self {
        let (sender, receiver) = oneshot::channel();
        Self {
            sender: Rc::new(RefCell::new(Some(sender))),
            receiver: receiver.shared(),
        }
    }
}

impl Completion {
    pub(crate) fn same_attempt(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.sender, &other.sender)
    }

    pub(crate) fn is_current_in(&self, lanes: &crate::tx_lanes::TxLanes, route: &crate::binding::DbRoute) -> bool {
        lanes
            .transaction_completion(route)
            .is_some_and(|current| self.same_attempt(&current))
    }

    pub(super) fn is_current(&self, route: &crate::binding::DbRoute) -> bool {
        crate::tx_lanes::with(|lanes| self.is_current_in(lanes, route))
    }

    pub(super) fn finish(&self, result: Driven) {
        let sender = self.sender.borrow_mut().take();
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    pub(crate) fn abandon(&self) {
        self.finish(Self::missing_result());
    }

    pub(super) async fn wait(&self) -> Driven {
        self.receiver
            .clone()
            .await
            .unwrap_or_else(|_| Self::missing_result())
    }

    fn missing_result() -> Driven {
        Driven {
            reply: Some(Err(TxProtocolError::TransactionNotReady)),
            error: Some(crate::error::DbError::internal(
                "db.transaction: the transaction was retired without a terminal result",
            )),
            ..Driven::default()
        }
    }
}
