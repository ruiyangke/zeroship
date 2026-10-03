use super::{Error, PolicyObservation};
use futures::channel::oneshot;
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use zeroship_core::app_id::AppId;

/// One manager process's observations of app policy, shared by every HTTP
/// thread that serves policy from it.
///
/// SHARED, AND THAT IS THE POINT. A grant is capped at the absolute expiry of
/// the observation it was issued from, and a worker host retires its policy
/// epoch - cancelling every operation bound to it - when a renewal's deadline
/// lands earlier than the one it already holds by more than the round trip that
/// reconstructed it. An observation's window starts when it was read, so two
/// observers of the same app hold windows as far apart as their reads, and a
/// host's leases land on whichever thread accepted the connection: alternating
/// between two windows reads as a shortening that never happened. One
/// observation per app per process keeps the cap moving in one direction, which
/// is what that fence assumes.
#[derive(Clone, Debug)]
pub struct PolicyObservations(Arc<Cache>);

impl PolicyObservations {
    /// Bound the number of apps whose observations this process retains.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self(Arc::new(Cache::new(capacity)))
    }

    pub(super) fn cache(&self) -> &Cache {
        &self.0
    }
}

#[derive(Debug)]
pub(super) struct Cache {
    entries: Mutex<HashMap<AppId, Entry>>,
    capacity: NonZeroUsize,
}
#[derive(Debug)]
struct Entry {
    identity: Arc<()>,
    observation: Option<PolicyObservation>,
    used_at: Instant,
    /// Callers that arrived while this app's refresh was in flight, to be
    /// answered from its result. Empty unless a refresh is pending.
    waiters: Vec<oneshot::Sender<Result<PolicyObservation, Error>>>,
}

pub(super) enum Reservation<'a> {
    Cached(PolicyObservation),
    Refresh(Ticket<'a>),
    /// A refresh for this app is already in flight; wait for its observation
    /// rather than refuse a request that the next observation will answer.
    Wait(oneshot::Receiver<Result<PolicyObservation, Error>>),
}
pub(super) struct Ticket<'a> {
    cache: &'a Cache,
    app: AppId,
    identity: Arc<()>,
    pending: bool,
}

impl Cache {
    pub(super) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    // A poisoned map is an infrastructure failure, never a durable refusal:
    // report it the way every other unavailable source read is reported.
    fn entries(&self) -> Result<MutexGuard<'_, HashMap<AppId, Entry>>, Error> {
        self.entries.lock().map_err(|_| Error::Unavailable)
    }

    pub(super) fn reserve(&self, app: &AppId) -> Result<Reservation<'_>, Error> {
        let mut entries = self.entries()?;
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(app) {
            match &entry.observation {
                Some(observation) if observation.expires_at() > now => {
                    entry.used_at = now;
                    let observation = observation.clone();
                    drop(entries);
                    return Ok(Reservation::Cached(observation));
                }
                None => {
                    let (waiter, waiting) = oneshot::channel();
                    entry.waiters.push(waiter);
                    drop(entries);
                    return Ok(Reservation::Wait(waiting));
                }
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
        let identity = Arc::new(());
        entries.insert(
            app.clone(),
            Entry {
                identity: identity.clone(),
                observation: None,
                used_at: now,
                waiters: Vec::new(),
            },
        );
        drop(entries);
        Ok(Reservation::Refresh(Ticket {
            cache: self,
            app: app.clone(),
            identity,
            pending: true,
        }))
    }

    pub(super) fn invalidate(&self, app: &AppId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(app);
        }
    }

    pub(super) fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error> {
        let entries = self.entries()?;
        let expires_at = entries
            .get(observation.app_id())
            .and_then(|entry| entry.observation.as_ref())
            .filter(|current| current.same_observation(observation))
            .map(PolicyObservation::expires_at)
            .filter(|expires_at| *expires_at > Instant::now())
            .ok_or(Error::Unavailable)?;
        drop(entries);
        Ok(expires_at)
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
        // A refusal below drops this ticket, whose own cleanup takes the same
        // lock. The guard is a local and `self` is a parameter, so the guard
        // goes first on every path out of here; `invalidation_during_refresh_
        // fences_old_completion_and_its_cleanup` drives that path, and it would
        // hang rather than fail if that order ever changed.
        let mut entries = self.cache.entries()?;
        let entry = entries
            .get_mut(&self.app)
            .filter(|entry| Arc::ptr_eq(&entry.identity, &self.identity))
            .ok_or(Error::Unavailable)?;
        entry.observation = Some(observation.clone());
        let waiters = std::mem::take(&mut entry.waiters);
        drop(entries);
        self.pending = false;
        for waiter in waiters {
            let _ = waiter.send(Ok(observation.clone()));
        }
        Ok(observation)
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if self.pending {
            let Ok(mut entries) = self.cache.entries.lock() else {
                return;
            };
            let waiters = match entries.get_mut(&self.app) {
                Some(entry) if Arc::ptr_eq(&entry.identity, &self.identity) => {
                    let waiters = std::mem::take(&mut entry.waiters);
                    entries.remove(&self.app);
                    waiters
                }
                _ => Vec::new(),
            };
            drop(entries);
            for waiter in waiters {
                let _ = waiter.send(Err(Error::Unavailable));
            }
        }
    }
}

#[cfg(test)]
mod tests;
