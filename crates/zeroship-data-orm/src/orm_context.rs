//! State owned by an ORM host. Scoped futures restore this context on every poll
//! and during cancellation, including detached transaction cleanup.
use crate::{
    binding::DbBinding,
    protection::{mask_policy::MaskPolicy, protection_floor::ProtectionFloor},
    schema_cache::SchemaCache,
    tx_lanes::TxLanes,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

#[derive(Clone)]
pub struct OrmContext(Rc<State>);
#[derive(Default)]
struct State {
    schemas: RefCell<SchemaCache>,
    policies: RefCell<HashMap<DbBinding, MaskPolicy>>,
    floors: RefCell<HashMap<DbBinding, Rc<ProtectionFloor>>>,
    lanes: RefCell<TxLanes>,
}
impl std::fmt::Debug for OrmContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrmContext").finish_non_exhaustive()
    }
}
impl Default for OrmContext {
    fn default() -> Self {
        Self::new()
    }
}
impl OrmContext {
    pub fn new() -> Self {
        Self(Rc::new(State::default()))
    }
    /// Execute synchronous host setup in this context.
    pub fn with<T>(&self, action: impl FnOnce() -> T) -> T {
        ACTIVE.set(self, action)
    }
    /// Bind asynchronous work and its cancellation to this owner.
    pub fn scope<F: Future>(&self, future: F) -> Scoped<F> {
        Scoped {
            owner: self.clone(),
            future: Some(Box::pin(future)),
        }
    }
    pub(crate) fn schemas<T>(&self, f: impl FnOnce(&SchemaCache) -> T) -> T {
        f(&self.0.schemas.borrow())
    }
    pub(crate) fn schemas_mut<T>(&self, f: impl FnOnce(&mut SchemaCache) -> T) -> T {
        f(&mut self.0.schemas.borrow_mut())
    }
    #[cfg(test)]
    pub(crate) fn policies<T>(&self, f: impl FnOnce(&HashMap<DbBinding, MaskPolicy>) -> T) -> T {
        f(&self.0.policies.borrow())
    }
    pub(crate) fn policies_mut<T>(
        &self,
        f: impl FnOnce(&mut HashMap<DbBinding, MaskPolicy>) -> T,
    ) -> T {
        f(&mut self.0.policies.borrow_mut())
    }
    pub(crate) fn floors<T>(
        &self,
        f: impl FnOnce(&HashMap<DbBinding, Rc<ProtectionFloor>>) -> T,
    ) -> T {
        f(&self.0.floors.borrow())
    }
    pub(crate) fn floors_mut<T>(
        &self,
        f: impl FnOnce(&mut HashMap<DbBinding, Rc<ProtectionFloor>>) -> T,
    ) -> T {
        f(&mut self.0.floors.borrow_mut())
    }
    pub(crate) fn lanes<T>(&self, f: impl FnOnce(&TxLanes) -> T) -> T {
        f(&self.0.lanes.borrow())
    }
    pub(crate) fn lanes_mut<T>(&self, f: impl FnOnce(&mut TxLanes) -> T) -> T {
        f(&mut self.0.lanes.borrow_mut())
    }
}

// The worker host shares a context across its dispatches. Standalone Database
// instances install their own owner while running. No borrow crosses a poll.
scoped_tls::scoped_thread_local!(static ACTIVE: OrmContext);
thread_local! { static HOST: OrmContext = OrmContext::new(); }
pub(crate) fn current() -> OrmContext {
    if ACTIVE.is_set() {
        ACTIVE.with(Clone::clone)
    } else {
        HOST.with(Clone::clone)
    }
}

#[must_use = "scoped work runs when polled"]
pub struct Scoped<F: Future> {
    owner: OrmContext,
    future: Option<Pin<Box<F>>>,
}
impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.owner.with(|| {
            this.future
                .as_mut()
                .expect("scoped future exists until drop")
                .as_mut()
                .poll(cx)
        })
    }
}
impl<F: Future> Drop for Scoped<F> {
    fn drop(&mut self) {
        self.owner.with(|| drop(self.future.take()));
    }
}
pub(crate) fn spawn<F: Future + 'static>(future: F) -> compio::runtime::JoinHandle<F::Output> {
    compio::runtime::spawn(current().scope(future))
}

impl<F: Future> std::fmt::Debug for Scoped<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scoped")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scoped_tasks_drop_after_the_thread_host_is_destroyed() {
        std::thread::spawn(|| {
            thread_local! { static RUNTIME: compio::runtime::Runtime = compio::runtime::Runtime::new().unwrap(); }
            RUNTIME.with(|runtime| runtime.block_on(async {
                // Initialize the host after the runtime, reversing their drop order.
                let owner = super::current();
                super::spawn(owner.scope(std::future::pending::<()>())).detach();
            }));
        }).join().expect("runtime teardown must drop scoped tasks safely");
    }
}
