//! Isolate-owned download sources. Only quota counters are shared across
//! isolates of the same app on a worker thread. Dropping an isolate releases
//! its sources and permits; a new deploy cannot inherit old stream handles.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use zeroship_storage::backend::BoxByteStream;

pub(crate) type StreamSlot = Rc<RefCell<StreamEntry>>;

pub(crate) struct StreamEntry {
    pub source: Option<BoxByteStream>,
    _permit: Permit,
}

struct Budget {
    app_id: String,
    live: Cell<usize>,
}

impl Drop for Budget {
    fn drop(&mut self) {
        let _ = BUDGETS.try_with(|budgets| {
            budgets.borrow_mut().remove(&self.app_id);
        });
    }
}

struct Permit(Rc<Budget>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.live.set(self.0.live.get() - 1);
    }
}

thread_local! {
    static BUDGETS: RefCell<HashMap<String, Weak<Budget>>> = RefCell::new(HashMap::new());
}

pub(crate) struct LiveStreams {
    streams: RefCell<HashMap<u32, StreamSlot>>,
    next_id: Cell<u32>,
    budget: Rc<Budget>,
    cap: usize,
}

impl LiveStreams {
    pub fn new(app_id: &str) -> Self {
        let budget = BUDGETS.with(|budgets| {
            let mut budgets = budgets.borrow_mut();
            if let Some(budget) = budgets.get(app_id).and_then(Weak::upgrade) {
                return budget;
            }
            let budget = Rc::new(Budget {
                app_id: app_id.to_owned(),
                live: Cell::new(0),
            });
            budgets.insert(app_id.to_owned(), Rc::downgrade(&budget));
            budget
        });
        Self {
            streams: RefCell::new(HashMap::new()),
            next_id: Cell::new(0),
            budget,
            cap: crate::limits::max_live_get_streams_per_app(),
        }
    }

    pub fn open(&self, source: BoxByteStream) -> Result<u32, String> {
        if self.budget.live.get() >= self.cap {
            return Err(format!(
                "storage: too many live download streams ({} max per app); read one to the end, or cancel it",
                self.cap,
            ));
        }
        let mut streams = self.streams.borrow_mut();
        let id = loop {
            self.next_id.set(self.next_id.get().wrapping_add(1).max(1));
            if !streams.contains_key(&self.next_id.get()) {
                break self.next_id.get();
            }
        };
        self.budget.live.set(self.budget.live.get() + 1);
        streams.insert(
            id,
            Rc::new(RefCell::new(StreamEntry {
                source: Some(source),
                _permit: Permit(Rc::clone(&self.budget)),
            })),
        );
        Ok(id)
    }

    pub fn slot(&self, id: u32) -> Option<StreamSlot> {
        self.streams.borrow().get(&id).cloned()
    }

    pub fn close(&self, id: u32) {
        self.streams.borrow_mut().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_storage::backend::{ChunkResult, ChunkSource};

    struct Idle;
    #[async_trait::async_trait(?Send)]
    impl ChunkSource for Idle {
        async fn next_chunk(&mut self) -> Option<ChunkResult> {
            None
        }
    }

    #[test]
    fn stream_ownership_is_per_isolate_even_for_the_same_app() {
        let a = LiveStreams::new("app_a");
        let b = LiveStreams::new("app_a");
        let id = a.open(Box::new(Idle)).unwrap();
        assert!(b.slot(id).is_none());
        b.close(id);
        assert!(a.slot(id).is_some());
    }

    #[test]
    fn the_cap_is_shared_by_isolates_of_the_same_app() {
        let a = LiveStreams::new("app_b");
        let b = LiveStreams::new("app_b");
        let c = LiveStreams::new("app_c");
        for _ in 0..a.cap {
            a.open(Box::new(Idle)).unwrap();
        }
        assert!(a.open(Box::new(Idle)).is_err());
        assert!(b.open(Box::new(Idle)).is_err());
        assert!(c.open(Box::new(Idle)).is_ok());
        drop(a);
        assert!(b.open(Box::new(Idle)).is_ok());
    }

    #[test]
    fn dropping_the_registry_releases_sources_and_budget_entries() {
        struct Tracked(Rc<Cell<bool>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        #[async_trait::async_trait(?Send)]
        impl ChunkSource for Tracked {
            async fn next_chunk(&mut self) -> Option<ChunkResult> {
                None
            }
        }
        let dropped = Rc::new(Cell::new(false));
        let registry = LiveStreams::new("app_drop");
        registry
            .open(Box::new(Tracked(Rc::clone(&dropped))))
            .unwrap();
        drop(registry);
        assert!(dropped.get());
        BUDGETS.with(|budgets| assert!(!budgets.borrow().contains_key("app_drop")));
    }

    #[test]
    fn closing_a_pulled_stream_retains_its_permit_until_the_pull_finishes() {
        let registry = LiveStreams::new("app_pull");
        let id = registry.open(Box::new(Idle)).unwrap();
        let slot = registry.slot(id).unwrap();
        registry.close(id);
        assert_eq!(registry.budget.live.get(), 1);
        drop(slot);
        assert_eq!(registry.budget.live.get(), 0);
    }
}
