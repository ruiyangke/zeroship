use super::ConnectionFactory;
use crate::{backend::BackendHandle, encryption::ProjectKeySource, error::DbError};
use futures::{
    future::{LocalBoxFuture, Shared},
    FutureExt,
};
use std::{cell::RefCell, rc::Rc};

type Opening = Shared<LocalBoxFuture<'static, Result<BackendHandle, DbError>>>;
#[derive(Default)]
struct State {
    backend: Option<BackendHandle>,
    opening: Option<(u64, Opening)>,
    generation: u64,
}

/// Thread-local lazy connection. Clones share backend initialization and reuse.
/// Cancelling a waiter preserves the pending open for the next caller. A failed
/// open is reported to its waiters and a subsequent call can retry.
#[derive(Clone)]
pub struct LocalConnection {
    factory: ConnectionFactory,
    state: Rc<RefCell<State>>,
}
impl std::fmt::Debug for LocalConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalConnection")
            .field("factory", &self.factory)
            .finish_non_exhaustive()
    }
}
impl LocalConnection {
    pub fn new(factory: ConnectionFactory) -> Self {
        Self {
            factory,
            state: Rc::new(RefCell::new(State::default())),
        }
    }
    pub fn factory(&self) -> &ConnectionFactory {
        &self.factory
    }
    pub fn backend(&self) -> Option<BackendHandle> {
        self.state.borrow().backend.clone()
    }
    /// Bind a host-supplied backend to the same identity used by lazy callers.
    pub fn from_backend(
        factory: ConnectionFactory,
        backend: BackendHandle,
    ) -> Result<Self, DbError> {
        if factory.dialect() != backend.dialect() {
            return Err(DbError::config(
                "backend_dialect_mismatch",
                "installed backend does not match its factory dialect",
            ));
        }
        Ok(Self {
            factory,
            state: Rc::new(RefCell::new(State {
                backend: Some(backend),
                ..State::default()
            })),
        })
    }
    pub async fn ensure(&self, keys: ProjectKeySource) -> Result<BackendHandle, DbError> {
        let (generation, opening) = {
            let mut state = self.state.borrow_mut();
            if let Some(backend) = &state.backend {
                return Ok(backend.clone());
            }
            if let Some(opening) = &state.opening {
                opening.clone()
            } else {
                let factory = self.factory.clone();
                let opening = async move { factory.connect(keys).await }
                    .boxed_local()
                    .shared();
                state.generation += 1;
                let pending = (state.generation, opening);
                state.opening = Some(pending.clone());
                pending
            }
        };
        let result = opening.await;
        let mut state = self.state.borrow_mut();
        if state
            .opening
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            state.opening = None;
            if let Ok(backend) = &result {
                state.backend = Some(backend.clone());
            }
        }
        result
    }
}
