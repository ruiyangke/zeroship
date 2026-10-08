//! A policy source that answers exactly the observations a case grants.
//!
//! It refuses every app it was not granted, so a visit to an app the case never
//! arranged is refused rather than served by a stub that answers for anything a
//! claim or a lane happens to enumerate.

use futures::future::LocalBoxFuture;
use std::{
    cell::RefCell,
    time::{Duration, Instant},
};
use zeroship_core::{workflow_policy::AppPolicy, AppId, ZoneId};
use zeroship_workflow_manager::{
    policy::{PolicyObservation, PolicySource},
    Error,
};

/// How long every granted observation stays valid.
pub const VALIDITY: Duration = Duration::from_mins(10);

#[derive(Debug, Default)]
pub struct GrantedPolicies {
    current: RefCell<Vec<PolicyObservation>>,
    /// Observations that replace an app's current one once it has been read.
    next: RefCell<Vec<PolicyObservation>>,
    /// Apps whose observation never answers.
    stalled: RefCell<Vec<AppId>>,
}

impl GrantedPolicies {
    /// Never answer for `app`, so a visit to it lasts until its caller's
    /// deadline cuts the observation.
    pub fn stall(&self, app: &AppId) {
        self.stalled.borrow_mut().push(app.clone());
    }

    /// Answer `app` with `policy` in `zone`, replacing any earlier grant.
    pub fn grant(&self, app: &AppId, zone: &ZoneId, policy: AppPolicy) {
        install(&self.current, observation(app, zone, policy, false, 7));
    }

    /// Answer `app` as deleted, keeping its zone and policy.
    pub fn delete(&self, app: &AppId) {
        let current = self
            .find(app)
            .expect("only a granted app can be deleted");
        install(
            &self.current,
            observation(
                app,
                current.execution_zone_id(),
                current.policy().clone(),
                true,
                current.revision().get(),
            ),
        );
    }

    /// Answer `app` with its current grant once more, and with `policy`, at the
    /// next revision, from the read after that: a policy change landing between
    /// two reads of one request.
    pub fn change_after_next_read(&self, app: &AppId, policy: AppPolicy) {
        let current = self.find(app).expect("only a granted app can change");
        install(
            &self.next,
            observation(
                app,
                current.execution_zone_id(),
                policy,
                current.deleted(),
                current.revision().get() + 1,
            ),
        );
    }

    /// Answer `app` with its current grant once more, and as deleted from the
    /// read after that: a deletion landing between two reads of one request.
    pub fn delete_after_next_read(&self, app: &AppId) {
        let current = self.find(app).expect("only a granted app can be deleted");
        install(
            &self.next,
            observation(
                app,
                current.execution_zone_id(),
                current.policy().clone(),
                true,
                current.revision().get(),
            ),
        );
    }

    fn find(&self, app: &AppId) -> Option<PolicyObservation> {
        self.current
            .borrow()
            .iter()
            .find(|observed| observed.app_id() == app)
            .cloned()
    }
}

fn observation(
    app: &AppId,
    zone: &ZoneId,
    policy: AppPolicy,
    deleted: bool,
    revision: i64,
) -> PolicyObservation {
    PolicyObservation::new(
        app.clone(),
        revision.try_into().unwrap(),
        policy,
        zone.clone(),
        deleted,
        Instant::now() + VALIDITY,
    )
    .unwrap()
}

fn install(slot: &RefCell<Vec<PolicyObservation>>, observation: PolicyObservation) {
    let mut granted = slot.borrow_mut();
    granted.retain(|observed| observed.app_id() != observation.app_id());
    granted.push(observation);
}

impl PolicySource for GrantedPolicies {
    fn observe<'a>(&'a self, app: &'a AppId) -> LocalBoxFuture<'a, Result<PolicyObservation, Error>> {
        Box::pin(async move {
            if self.stalled.borrow().contains(app) {
                return std::future::pending().await;
            }
            let answer = self.find(app).ok_or(Error::Denied)?;
            let next = self
                .next
                .borrow()
                .iter()
                .position(|observed| observed.app_id() == app);
            if let Some(index) = next {
                let changed = self.next.borrow_mut().remove(index);
                install(&self.current, changed);
            }
            Ok(answer)
        })
    }
}
