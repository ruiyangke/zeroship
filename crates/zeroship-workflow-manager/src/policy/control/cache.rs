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
    /// When the current observation is read again although it is still valid:
    /// halfway through its validity. A grant is capped at the expiry of the
    /// observation it is issued from, so an observation served until its last
    /// moment would issue grants with almost nothing left; reading it again
    /// ahead of expiry keeps every grant at least half a window long while the
    /// source answers.
    refresh_at: Instant,
    /// A read ahead of expiry is in flight. Every other caller keeps the
    /// current observation meanwhile, and keeps it if that read fails.
    refreshing: bool,
    used_at: Instant,
    /// Callers that arrived while this app's refresh was in flight, to be
    /// answered from its result. Empty unless a refresh is pending.
    waiters: Vec<oneshot::Sender<Result<PolicyObservation, Error>>>,
}

pub(super) enum Reservation<'a> {
    Cached(PolicyObservation),
    Refresh(Ticket<'a>),
    /// The current observation is past the point it is read again. This
    /// caller reads the source; if that read fails, the current observation,
    /// still valid, answers it.
    Ahead(Ticket<'a>, PolicyObservation),
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
                    if now < entry.refresh_at || entry.refreshing {
                        drop(entries);
                        return Ok(Reservation::Cached(observation));
                    }
                    let identity = Arc::new(());
                    entry.identity = identity.clone();
                    entry.refreshing = true;
                    drop(entries);
                    return Ok(Reservation::Ahead(
                        Ticket {
                            cache: self,
                            app: app.clone(),
                            identity,
                            pending: true,
                        },
                        observation,
                    ));
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
                refresh_at: now,
                refreshing: false,
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
        let now = Instant::now();
        entry.refresh_at = now + observation.expires_at().saturating_duration_since(now) / 2;
        entry.refreshing = false;
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
                // A read ahead of expiry that did not complete leaves the
                // current observation serving until it expires; the next
                // caller past its refresh point reads again.
                Some(entry)
                    if Arc::ptr_eq(&entry.identity, &self.identity)
                        && entry
                            .observation
                            .as_ref()
                            .is_some_and(|current| current.expires_at() > Instant::now()) =>
                {
                    entry.refreshing = false;
                    Vec::new()
                }
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
