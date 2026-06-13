pub mod authz_fixture;

use zeroship_control::Registry;

/// Idempotently seed the built-in plan tiers (free/pro/unlimited) into the test
/// DB's plan catalog so `create_app`/`set_plan` (which validate `plan_id`
/// against `zeroship.plans` since PR4) accept the built-in ids. A no-op on a
/// re-run (ON CONFLICT DO UPDATE on deterministic `pln_…` ids). Tests that
/// create apps with [`zeroship_control::bootstrap_console::free_plan_id`] call
/// this in their setup first.
#[allow(dead_code)]
pub async fn ensure_builtin_plans(registry: &Registry) {
    zeroship_control::bootstrap_console::seed_plans(registry)
        .await
        .expect("seed built-in plans for test");
}
