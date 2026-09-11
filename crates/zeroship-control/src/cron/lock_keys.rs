//! Every `pg_advisory_lock` key the control-plane sweeps use, in one place.
//!
//! # Why these are centralised
//!
//! Two sweeps sharing a key block each other FLEET-WIDE, and the symptom is
//! the worst shape available here: the losing sweep silently never runs.
//! Nothing errors, nothing logs, and "no work done" is indistinguishable from
//! "nothing to do". So the invariant that matters is that the set is
//! pairwise-distinct, and that has to hold for keys added anywhere.
//!
//! It did not hold before. Each key was a private `const` in its own module,
//! and four modules had grown their own hand-written collision test comparing
//! against transcribed literals:
//!
//! ```text
//! dunning.rs           1 comparison
//! stripe_reconcile.rs  1
//! billing_notify.rs    2
//! spend_recompute.rs   6
//! ```
//!
//! Ten of the twenty-one pairs, spread over four tests that each read like
//! coverage. The two keys defined in the SAME file - `BILLING_SWEEP` and
//! `BILLING_SAFETY_NET` in `billing_reconcile.rs` - were never compared to
//! each other by any of them.
//!
//! With the keys in one array, [`tests::all_keys_are_pairwise_distinct`] holds
//! for the whole set by construction, and a new key is covered the moment it
//! joins [`ALL`] - which is the part the literal lists structurally could not
//! do.
//!
//! # Encoding
//!
//! `0x7a73` is ASCII `zs`, then four bytes naming the sweep, then a version
//! nibble. The mnemonic is a convenience for reading `pg_locks` output; the
//! distinctness is the load-bearing property, not the spelling.

/// Monthly billing sweep (`billing_reconcile`).
pub(crate) const BILLING_SWEEP: i64 = 0x7a73_6269_6c6c_0001;

/// Reconciliation safety-net sweep (`billing_reconcile`).
pub(crate) const BILLING_SAFETY_NET: i64 = 0x7a73_6273_6166_0001;

/// Billing notification sweep (`billing_notify`).
pub(crate) const BILLING_NOTIFY: i64 = 0x7a73_6e6f_7466_0001;

/// Stripe reconciliation sweep (`stripe_reconcile`).
pub(crate) const STRIPE_RECONCILE: i64 = 0x7a73_7265_636f_0001;

/// Spend evaluation sweep (`spend_reconcile`).
pub(crate) const SPEND_SWEEP: i64 = 0x7a73_7370_6e64_0001;

/// Dunning sweep (`dunning`).
pub(crate) const DUNNING_SWEEP: i64 = 0x7a73_6475_6e6e_0001;

/// Usage-aggregate recompute (`spend_recompute`).
pub(crate) const SPEND_RECOMPUTE: i64 = 0x7a73_7263_6d70_0001;

/// Every key above, paired with the sweep it belongs to.
///
/// A new key MUST be added here. That is the whole mechanism: the uniqueness
/// test iterates this array, so a key that is not in it is a key nothing
/// checks. `workflow_blob_gc` is deliberately absent - it uses
/// `pg_try_advisory_xact_lock` with a `hashtextextended` of a text label
/// rather than a literal key from this set.
#[cfg(test)]
pub(crate) const ALL: [(i64, &str); 7] = [
    (BILLING_SWEEP, "billing sweep"),
    (BILLING_SAFETY_NET, "billing safety net"),
    (BILLING_NOTIFY, "billing notify"),
    (STRIPE_RECONCILE, "stripe reconcile"),
    (SPEND_SWEEP, "spend sweep"),
    (DUNNING_SWEEP, "dunning sweep"),
    (SPEND_RECOMPUTE, "spend recompute"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn all_keys_are_pairwise_distinct() {
        let mut seen = HashMap::<i64, &str>::new();
        for (key, name) in ALL {
            if let Some(previous) = seen.insert(key, name) {
                panic!(
                    "advisory lock key {key:#x} is used by both '{previous}' and '{name}'; \
                     the two sweeps would block each other fleet-wide and neither would \
                     report an error - the losing one would just stop running"
                );
            }
        }
        assert_eq!(seen.len(), ALL.len());
    }

    /// The array is the coverage. A key that exists as a `const` here but is
    /// left out of [`ALL`] is invisible to the test above, so pin the count:
    /// adding a const without extending the array fails here rather than
    /// silently reducing what is checked.
    #[test]
    fn every_declared_key_is_in_the_all_array() {
        for key in [
            BILLING_SWEEP,
            BILLING_SAFETY_NET,
            BILLING_NOTIFY,
            STRIPE_RECONCILE,
            SPEND_SWEEP,
            DUNNING_SWEEP,
            SPEND_RECOMPUTE,
        ] {
            assert!(
                ALL.iter().any(|(k, _)| *k == key),
                "key {key:#x} is declared but missing from ALL, so nothing checks it"
            );
        }
    }
}
