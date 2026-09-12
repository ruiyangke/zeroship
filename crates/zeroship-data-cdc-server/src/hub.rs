//! Bounded fanout. A lagging connection is disconnected and must resnapshot.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use zeroship_data_cdc_wire::Event;

#[derive(Default)]
pub(crate) struct Hub {
    apps: RefCell<HashMap<String, App>>,
    next_id: RefCell<u64>,
}

struct App {
    generation: u64,
    ready: bool,
    subscribers: HashMap<u64, flume::Sender<Event>>,
    shutdown: flume::Sender<()>,
}

pub(crate) struct Lease {
    hub: Rc<Hub>,
    app: String,
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
        app: &str,
        max_apps: usize,
        max_clients: usize,
        queue: usize,
    ) -> Result<(Lease, Option<Start>), &'static str> {
        let mut apps = self.apps.borrow_mut();
        if !apps.contains_key(app) && apps.len() >= max_apps {
            return Err("relay app capacity exhausted");
        }
        let mut next_id = self.next_id.borrow_mut();
        *next_id = next_id.checked_add(1).ok_or("relay identity exhausted")?;
        let id = *next_id;
        let mut start = None;
        let state = apps.entry(app.into()).or_insert_with(|| {
            let (shutdown, receiver) = flume::bounded(1);
            start = Some(Start {
                generation: id,
                shutdown: receiver,
            });
            App {
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
                app: app.into(),
                id,
                events,
            },
            start,
        ))
    }

    pub fn ready(&self, app: &str, generation: u64) {
        if let Some(state) = self.apps.borrow_mut().get_mut(app) {
            if state.generation != generation {
                return;
            }
            state.ready = true;
        }
        self.publish(app, generation, Event::Ready);
    }

    pub fn publish(&self, app: &str, generation: u64, event: Event) {
        let mut apps = self.apps.borrow_mut();
        let Some(state) = apps.get_mut(app) else {
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

    pub fn end(&self, app: &str, generation: u64) {
        let mut apps = self.apps.borrow_mut();
        if apps
            .get(app)
            .is_some_and(|state| state.generation == generation)
        {
            apps.remove(app);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut apps = self.hub.apps.borrow_mut();
        if let Some(state) = apps.get_mut(&self.app) {
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

    #[test]
    fn slow_receiver_does_not_block_healthy_receiver() {
        let hub = Rc::new(Hub::default());
        let (slow, start) = hub.subscribe("app", 1, 4, 1).unwrap();
        let start = start.unwrap();
        let (fast, duplicate) = hub.subscribe("app", 1, 4, 1).unwrap();
        assert!(duplicate.is_none());
        hub.ready("app", start.generation);
        assert_eq!(fast.events.try_recv().unwrap(), Event::Ready);
        hub.publish("app", start.generation, Event::Resync);
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
        let (old, start) = hub.subscribe("app", 1, 2, 2).unwrap();
        let generation = start.unwrap().generation;
        hub.end("app", generation);
        let (new, start) = hub.subscribe("app", 1, 2, 2).unwrap();
        let generation2 = start.unwrap().generation;
        drop(old);
        hub.ready("app", generation2);
        let (late, _) = hub.subscribe("app", 1, 2, 2).unwrap();
        hub.end("app", generation);
        hub.publish("app", generation2, Event::Resync);
        for lease in [new, late] {
            assert_eq!(lease.events.try_recv().unwrap(), Event::Ready);
            assert_eq!(lease.events.try_recv().unwrap(), Event::Resync);
        }
    }
}
