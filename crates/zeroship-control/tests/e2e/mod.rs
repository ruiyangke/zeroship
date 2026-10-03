//! The end-to-end suites for `zeroship-control` that share the `main` binary.
//!
//! These launch real processes. The workflow process contracts and the
//! app/database decoupling exercise keep their own `[[test]]` targets because
//! their template cloning and platform-zone writes cannot share this binary.

mod billing_pipeline_redpanda_e2e;
mod control_boot_test;
