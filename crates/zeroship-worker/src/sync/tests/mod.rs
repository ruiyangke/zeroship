use super::*;
use zeroship_bundle::{BlobStore, Manifest};
use zeroship_core::net_policy::Verdict;
use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits, NetEgressEntry};

/// The database-to-capability half of a live binding set.
///
/// Everything a comparison that ignored the edge could see, so a case about a
/// rebind can SHOW that projection standing still across the change it detects,
/// rather than assert in prose that it would.
fn capabilities(
    set: &std::collections::BTreeMap<zeroship_core::DatabaseId, zeroship_core::types::LiveBinding>,
) -> std::collections::BTreeMap<
    &zeroship_core::DatabaseId,
    zeroship_core::database_role::DatabaseCapability,
> {
    set.iter()
        .map(|(database, live)| (database, live.capability))
        .collect()
}

mod bindings;
mod decision;
mod descriptor;
mod fixture;
mod reconcile;
mod transport;
