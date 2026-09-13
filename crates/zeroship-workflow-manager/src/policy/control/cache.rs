use super::{Error, PolicyObservation};
use std::{cell::RefCell, collections::HashMap, num::NonZeroUsize, rc::Rc, time::Instant};
use zeroship_core::app_id::AppId;

#[derive(Debug)]
pub(super) struct Cache {
    entries: RefCell<HashMap<AppId, Entry>>,
    capacity: NonZeroUsize,
}
#[derive(Debug)]
struct Entry {
    identity: Rc<()>,
    observation: Option<PolicyObservation>,
    used_at: Instant,
}

pub(super) enum Reservation<'a> {
    Cached(PolicyObservation),
    Refresh(Ticket<'a>),
}
pub(super) struct Ticket<'a> {
    cache: &'a Cache,
    app: AppId,
    identity: Rc<()>,
    pending: bool,
}

impl Cache {
    pub(super) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
            capacity,
        }
    }

    pub(super) fn reserve(&self, app: &AppId) -> Result<Reservation<'_>, Error> {
        let mut entries = self.entries.borrow_mut();
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(app) {
            match &entry.observation {
                Some(observation) if observation.expires_at() > now => {
                    entry.used_at = now;
                    return Ok(Reservation::Cached(observation.clone()));
                }
                None => return Err(Error::Unavailable),
                _ => {}
            }
        } else if entries.len() == self.capacity.get() {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.used_at)
                .map(|(app, _)| app.clone())
                .ok_or(Error::Unavailable)?;
            entries.remove(&oldest);
        }
        let identity = Rc::new(());
        entries.insert(
            app.clone(),
            Entry {
                identity: identity.clone(),
                observation: None,
                used_at: now,
            },
        );
        Ok(Reservation::Refresh(Ticket {
            cache: self,
            app: app.clone(),
            identity,
            pending: true,
        }))
    }

    pub(super) fn invalidate(&self, app: &AppId) {
        self.entries.borrow_mut().remove(app);
    }

    pub(super) fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error> {
        let entries = self.entries.borrow();
        let current = entries
            .get(observation.app_id())
            .and_then(|entry| entry.observation.as_ref())
            .filter(|current| current.same_observation(observation))
            .filter(|current| current.expires_at() > Instant::now())
            .ok_or(Error::Unavailable)?;
        Ok(current.expires_at())
    }
}

impl Ticket<'_> {
    pub(super) fn complete(
        mut self,
        observation: PolicyObservation,
    ) -> Result<PolicyObservation, Error> {
        if observation.app_id() != &self.app || observation.expires_at() <= Instant::now() {
            return Err(Error::Unavailable);
        }
        {
            let mut entries = self.cache.entries.borrow_mut();
            let entry = entries
                .get_mut(&self.app)
                .filter(|entry| Rc::ptr_eq(&entry.identity, &self.identity))
                .ok_or(Error::Unavailable)?;
            entry.observation = Some(observation.clone());
        }
        self.pending = false;
        Ok(observation)
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if self.pending {
            let mut entries = self.cache.entries.borrow_mut();
            if entries
                .get(&self.app)
                .is_some_and(|entry| Rc::ptr_eq(&entry.identity, &self.identity))
            {
                entries.remove(&self.app);
            }
        }
    }
}

#[cfg(test)]
mod tests;
