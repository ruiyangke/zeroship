//! Billing crate — pricing, spending limits, and invoice generation.
//!
//! Separated from quota/metering per spec v3.0. This crate:
//! - Computes cost from usage snapshots via PricingTable
//! - Manages spending limits via SpendingReconciler
//! - Sets SpendAction flags read by the quota enforcer
//! - Never runs on the hot request path

pub mod spend_action;
pub mod pricing;
pub mod reconciler;
