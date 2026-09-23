//! Bounded fanout. A lagging connection is disconnected and must resnapshot.
//!
//! **Subscribers are keyed on the app AND the database.** One app may hold a
//! live binding to several databases and two of them may each declare a
//! collection of the same name, so a stream keyed on the app alone would
//! deliver one database's invalidation to a subscriber of the other with
//! nothing raising.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use zeroship_data_cdc_wire::Event;

/// One subscription target: the tenant and the database it named.
///
/// Both halves are needed. The tenant alone is ambiguous once an app reaches
/// two databases; the database alone would merge two apps bound to one shared
/// database into a single fanout whose capacity and readiness they would
/// share.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct StreamKey {
    pub app: String,
    pub database: String,
}

impl StreamKey {
    pub fn new(app: &str, database: &str) -> Self {
        Self {
            app: app.to_owned(),
            database: database.to_owned(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Hub {
    streams: RefCell<HashMap<StreamKey, Stream>>,
    next_id: RefCell<u64>,
}

struct Stream {
    generation: u64,
    ready: bool,
    subscribers: HashMap<u64, flume::Sender<Event>>,
    shutdown: flume::Sender<()>,
}

pub(crate) struct Lease {
    hub: Rc<Hub>,
    key: StreamKey,
    id: u64,
    pub events: flume::Receiver<Event>,
}

pub(crate) struct Start {
    pub generation: u64,
    pub shutdown: flume::Receiver<()>,
}

impl Hub {
    pub fn subscribe(
        self: &Rc<Self>,
        key: &StreamKey,
        max_streams: usize,
        max_clients: usize,
        queue: usize,
    ) -> Result<(Lease, Option<Start>), &'static str> {
        let mut streams = self.streams.borrow_mut();
        if !streams.contains_key(key) && streams.len() >= max_streams {
            return Err("relay app capacity exhausted");
        }
        let mut next_id = self.next_id.borrow_mut();
        *next_id = next_id.checked_add(1).ok_or("relay identity exhausted")?;
        let id = *next_id;
        let mut start = None;
        let state = streams.entry(key.clone()).or_insert_with(|| {
            let (shutdown, receiver) = flume::bounded(1);
            start = Some(Start {
                generation: id,
                shutdown: receiver,
            });
            Stream {
                generation: id,
                ready: false,
                subscribers: HashMap::new(),
                shutdown,
            }
        });
        if state.subscribers.len() >= max_clients || queue == 0 {
            return Err("relay client capacity exhausted");
        }
        let (sender, events) = flume::bounded(queue);
        if state.ready {
            sender
                .try_send(Event::Ready)
                .map_err(|_| "relay queue unavailable")?;
        }
        state.subscribers.insert(id, sender);
        Ok((
            Lease {
                hub: self.clone(),
                key: key.clone(),
                id,
                events,
            },
            start,
        ))
    }

    pub fn ready(&self, key: &StreamKey, generation: u64) {
        if let Some(state) = self.streams.borrow_mut().get_mut(key) {
            if state.generation != generation {
                return;
            }
            state.ready = true;
        }
        self.publish(key, generation, Event::Ready);
    }

    pub fn publish(&self, key: &StreamKey, generation: u64, event: Event) {
        let mut streams = self.streams.borrow_mut();
        let Some(state) = streams.get_mut(key) else {
            return;
        };
        if state.generation != generation {
            return;
        }
        state
            .subscribers
            .retain(|_, sender| sender.try_send(event.clone()).is_ok());
        if state.subscribers.is_empty() {
            let _ = state.shutdown.try_send(());
        }
    }

    pub fn end(&self, key: &StreamKey, generation: u64) {
        let mut streams = self.streams.borrow_mut();
        if streams
            .get(key)
            .is_some_and(|state| state.generation == generation)
        {
            streams.remove(key);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut streams = self.hub.streams.borrow_mut();
        if let Some(state) = streams.get_mut(&self.key) {
            state.subscribers.remove(&self.id);
            if state.subscribers.is_empty() {
                let _ = state.shutdown.try_send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(app: &str, database: &str) -> StreamKey {
        StreamKey::new(app, database)
    }

    #[test]
    fn slow_receiver_does_not_block_healthy_receiver() {
        let hub = Rc::new(Hub::default());
        let stream = key("app", "dbs_one");
        let (slow, start) = hub.subscribe(&stream, 1, 4, 1).unwrap();
        let start = start.unwrap();
        let (fast, duplicate) = hub.subscribe(&stream, 1, 4, 1).unwrap();
        assert!(duplicate.is_none());
        hub.ready(&stream, start.generation);
        assert_eq!(fast.events.try_recv().unwrap(), Event::Ready);
        hub.publish(&stream, start.generation, Event::Resync);
        assert_eq!(fast.events.try_recv().unwrap(), Event::Resync);
        assert_eq!(slow.events.try_recv().unwrap(), Event::Ready);
        assert!(slow.events.is_disconnected());
        assert!(start.shutdown.is_empty());
        drop(fast);
        assert!(start.shutdown.try_recv().is_ok());
    }

    #[test]
    fn generation_fences_late_task_exit_and_ready_precedes_changes() {
        let hub = Rc::new(Hub::default());
        let stream = key("app", "dbs_one");
        let (old, start) = hub.subscribe(&stream, 1, 2, 2).unwrap();
        let generation = start.unwrap().generation;
        hub.end(&stream, generation);
        let (new, start) = hub.subscribe(&stream, 1, 2, 2).unwrap();
        let generation2 = start.unwrap().generation;
        drop(old);
        hub.ready(&stream, generation2);
        let (late, _) = hub.subscribe(&stream, 1, 2, 2).unwrap();
        hub.end(&stream, generation);
        hub.publish(&stream, generation2, Event::Resync);
        for lease in [new, late] {
            assert_eq!(lease.events.try_recv().unwrap(), Event::Ready);
            assert_eq!(lease.events.try_recv().unwrap(), Event::Resync);
        }
    }

    /// **One app's two databases are two streams.**
    ///
    /// The positive half is the control: without it a hub that dropped every
    /// event would satisfy the absence below.
    #[test]
    fn one_app_s_two_databases_do_not_share_a_fanout() {
        let hub = Rc::new(Hub::default());
        let first = key("app", "dbs_one");
        let second = key("app", "dbs_two");
        assert_eq!(first.app, second.app, "the control: one tenant");
        assert_ne!(first.database, second.database, "two databases");

        let (mine, my_start) = hub.subscribe(&first, 2, 2, 4).unwrap();
        let my_start = my_start.unwrap();
        let (theirs, their_start) = hub.subscribe(&second, 2, 2, 4).unwrap();
        let their_start =
            their_start.expect("the app's second database starts its own capture, not the first's");
        assert_ne!(
            my_start.generation, their_start.generation,
            "two streams, so two generations"
        );

        hub.ready(&first, my_start.generation);
        hub.publish(
            &first,
            my_start.generation,
            Event::Change {
                collection: "users".into(),
                operation: zeroship_data_cdc_wire::Operation::Insert,
            },
        );

        assert_eq!(mine.events.try_recv().unwrap(), Event::Ready);
        assert_eq!(
            mine.events.try_recv().unwrap(),
            Event::Change {
                collection: "users".into(),
                operation: zeroship_data_cdc_wire::Operation::Insert,
            },
            "the database that produced the change delivers it"
        );
        assert!(
            theirs.events.is_empty(),
            "a change on one database must not reach the other's subscriber"
        );
    }
}
