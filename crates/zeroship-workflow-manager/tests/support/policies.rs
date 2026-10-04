//! Policy sources for the manager contracts.
//!
//! The manager reads policy through [`PolicySource`]; a contract controls the
//! observation it hands a claim or a capacity lane. [`LocalPolicies`] answers
//! one permissive observation for any app, for lanes that never read policy.
#![allow(
    clippy::future_not_send,
    reason = "fixture providers run on their compio runtime"
)]

use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{app_id::AppId, workflow_policy::AppPolicy, zone_id::ZoneId};
use zeroship_workflow_manager::policy::{PolicyObservation, PolicySource};

const VALIDITY: Duration = Duration::from_secs(3600);

fn revision() -> zeroship_core::workflow_coordination::Revision {
    1.try_into().unwrap()
}

/// One permissive observation for every app, in the default zone, never
/// deleted. Lanes that do not exercise policy use it.
#[derive(Debug, Clone, Copy)]
pub struct LocalPolicies;

impl LocalPolicies {
    #[must_use]
    pub fn shared() -> Rc<dyn PolicySource> {
        Rc::new(Self)
    }
}

impl PolicySource for LocalPolicies {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PolicyObservation, zeroship_workflow_manager::Error>> + 'a>,
    > {
        Box::pin(async move {
            PolicyObservation::new(
                app.clone(),
                revision(),
                AppPolicy::default(),
                ZoneId::default_zone(),
                false,
                Instant::now() + VALIDITY,
            )
        })
    }
}

/// Policy a contract controls per app: its zone, deletion and `AppPolicy`.
#[derive(Debug, Default)]
pub struct Policies {
    apps: RefCell<BTreeMap<String, PolicyObservation>>,
    unavailable: RefCell<bool>,
    observed: RefCell<BTreeMap<String, usize>>,
}

impl Policies {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Record `app` in `zone` under the default policy.
    pub fn app(&self, app: &AppId, zone: &ZoneId) {
        self.with_policy(app, zone, AppPolicy::default());
    }

    /// Record `app` in `zone` under `policy`.
    pub fn with_policy(&self, app: &AppId, zone: &ZoneId, policy: AppPolicy) {
        let observation = PolicyObservation::new(
            app.clone(),
            revision(),
            policy,
            zone.clone(),
            false,
            Instant::now() + VALIDITY,
        )
        .unwrap();
        self.apps
            .borrow_mut()
            .insert(app.as_str().to_owned(), observation);
    }

    /// Terminal deletion, which Control records only for an archived app.
    pub fn delete(&self, app: &AppId) {
        let zone = self
            .apps
            .borrow()
            .get(app.as_str())
            .expect("known app")
            .execution_zone_id()
            .clone();
        let observation = PolicyObservation::new(
            app.clone(),
            revision(),
            AppPolicy::default(),
            zone,
            true,
            Instant::now() + VALIDITY,
        )
        .unwrap();
        self.apps
            .borrow_mut()
            .insert(app.as_str().to_owned(), observation);
    }

    /// Make every observation fail, as unreachable Control does.
    pub fn unavailable(&self) {
        *self.unavailable.borrow_mut() = true;
    }

    /// How many times `app` has been observed.
    #[must_use]
    pub fn observations(&self, app: &AppId) -> usize {
        self.observed.borrow().get(app.as_str()).copied().unwrap_or(0)
    }
}

impl PolicySource for Policies {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PolicyObservation, zeroship_workflow_manager::Error>> + 'a>,
    > {
        Box::pin(async move {
            *self
                .observed
                .borrow_mut()
                .entry(app.as_str().to_owned())
                .or_default() += 1;
            if *self.unavailable.borrow() {
                return Err(zeroship_workflow_manager::Error::Unavailable);
            }
            self.apps
                .borrow()
                .get(app.as_str())
                .cloned()
                .ok_or(zeroship_workflow_manager::Error::Denied)
        })
    }
}
