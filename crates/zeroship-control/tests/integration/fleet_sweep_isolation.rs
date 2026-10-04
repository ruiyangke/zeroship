//! A fleet-wide spend sweep is database-global and belongs on an isolated clone.

use zeroship_control::Registry;

/// The process-shared working database carries every sibling case's apps, so a
/// sweep there derives state for rows this case never seeded and aborts on a
/// sibling's plan that inherits the global FX. A sweep case must take a clone
/// [`crate::support::isolated_control_db`] hands it.
#[compio::test]
#[should_panic(expected = "a fleet-wide spend sweep is database-global")]
async fn a_fleet_sweep_on_the_process_shared_working_database_is_refused() {
    let shared = crate::support::require_control_db();
    let registry = Registry::new(&shared).await.expect("registry");
    let _ = crate::support::sweep_spend(registry, &shared).await;
}
