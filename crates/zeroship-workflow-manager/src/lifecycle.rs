//! Control's terminal app lifecycle as the manager observes it.
//!
//! Deletion is terminal: an app never returns from it. The closing lane asks
//! which of its candidates Control deleted and abandons their responsibility
//! instead of closing it.
#![expect(
    clippy::future_not_send,
    reason = "lifecycle reads stay on the manager's owning compio runtime"
)]

use crate::{Error, app_facts::AppFactsSource};
use std::{collections::BTreeSet, fmt::Debug, future::Future, pin::Pin, rc::Rc};
use zeroship_core::app_id::AppId;

/// Reports the apps whose terminal deletion Control recorded.
pub trait AppLifecycle: Debug {
    /// The subset of `apps` Control deleted. An app absent from Control's
    /// catalog is not reported deleted; only a recorded deletion abandons.
    fn deleted<'a>(
        &'a self,
        apps: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>>;
}

/// A host whose apps cannot be deleted: the local development host is its
/// app's only platform authority and keeps no Control catalog.
#[derive(Debug, Default, Clone, Copy)]
pub struct Undeletable;

impl AppLifecycle for Undeletable {
    fn deleted<'a>(
        &'a self,
        _: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>> {
        Box::pin(async { Ok(BTreeSet::new()) })
    }
}

/// Control's recorded deletions, read through the shared facts capability.
///
/// One exchange per pass rather than one per app: the lane hands its whole
/// candidate page over, and the answer names the rows Control has. The
/// watermark the answer also carries is ignored here on purpose - the lane
/// orders nothing against it and publishes no authority from it. Only the
/// policy ledger spends that guarantee.
#[derive(Debug, Clone)]
pub struct FactsLifecycle {
    facts: Rc<dyn AppFactsSource>,
}

impl FactsLifecycle {
    #[must_use]
    pub const fn new(facts: Rc<dyn AppFactsSource>) -> Self {
        Self { facts }
    }
}

impl AppLifecycle for FactsLifecycle {
    fn deleted<'a>(
        &'a self,
        apps: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>> {
        Box::pin(async move {
            if apps.is_empty() {
                return Ok(BTreeSet::new());
            }
            let response = self.facts.observe(apps).await?;
            response
                .apps
                .into_iter()
                .filter(|facts| facts.deleted)
                .map(|facts| {
                    if apps.contains(&facts.app_id) {
                        Ok(facts.app_id)
                    } else {
                        Err(Error::Storage)
                    }
                })
                .collect()
        })
    }
}
