//! PricingTable — multi-dimensional cost computation.
//!
//! Maps resource names to pricing rules. Computes cost in millicents
//! (1/1000 of a cent) from usage counter snapshots.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// A single pricing rule for a resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PricingRule {
    /// Flat rate: rate_millicents per `per_units` units consumed.
    Flat {
        rate_millicents: u64,
        per_units: u64,
    },
    /// Tiered (graduated) pricing.
    Tiered {
        tiers: Vec<PricingTier>,
    },
}

/// One tier in graduated pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingTier {
    /// Usage up to this amount uses this rate. None = unlimited (final tier).
    pub up_to: Option<u64>,
    /// Rate in millicents per `per_units`.
    pub rate_millicents: u64,
    /// Number of units the rate applies to.
    pub per_units: u64,
}

/// Pricing table — maps resource names to pricing rules.
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    pub rules: HashMap<String, PricingRule>,
}

impl PricingTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a flat pricing rule.
    pub fn add_flat(&mut self, resource: &str, rate_millicents: u64, per_units: u64) {
        self.rules.insert(
            resource.to_string(),
            PricingRule::Flat { rate_millicents, per_units },
        );
    }

    /// Add a tiered pricing rule.
    pub fn add_tiered(&mut self, resource: &str, tiers: Vec<PricingTier>) {
        self.rules.insert(
            resource.to_string(),
            PricingRule::Tiered { tiers },
        );
    }

    /// Compute total cost in millicents from a usage snapshot.
    pub fn compute_cost(&self, usage: &HashMap<String, u64>) -> u64 {
        let mut total_millicents: u64 = 0;
        for (resource, &amount) in usage {
            if let Some(rule) = self.rules.get(resource) {
                total_millicents = total_millicents.saturating_add(
                    compute_resource_cost(rule, amount),
                );
            }
            // Resources without pricing rules are free (not an error)
        }
        total_millicents
    }

    /// Compute cost breakdown per resource (for invoices/display).
    pub fn compute_breakdown(&self, usage: &HashMap<String, u64>) -> HashMap<String, u64> {
        let mut breakdown = HashMap::new();
        for (resource, &amount) in usage {
            if let Some(rule) = self.rules.get(resource) {
                let cost = compute_resource_cost(rule, amount);
                if cost > 0 {
                    breakdown.insert(resource.clone(), cost);
                }
            }
        }
        breakdown
    }

    /// Create a CF Workers-comparable default pricing table.
    pub fn cloudflare_comparable() -> Self {
        let mut table = Self::new();
        // $0.30/million requests = 300 millicents / 1M
        table.add_flat("requests", 300, 1_000_000);
        // $12.50/million CPU-ms = 12500 millicents / 1M
        table.add_flat("cpu_ms", 12500, 1_000_000); // per 1M milliseconds
        // $0.09/GB egress = 90 millicents / 1B bytes
        table.add_flat("egress_bytes", 90, 1_000_000_000);
        // $0.25/million db reads
        table.add_flat("db.reads", 250, 1_000_000);
        // $1.00/million db writes
        table.add_flat("db.writes", 1000, 1_000_000);
        // $0.25/GB-month storage
        table.add_flat("db.storage", 250, 1_000_000_000);
        table
    }
}

/// Compute cost for a single resource.
fn compute_resource_cost(rule: &PricingRule, amount: u64) -> u64 {
    match rule {
        PricingRule::Flat { rate_millicents, per_units } => {
            if *per_units == 0 { return 0; }
            // cost = amount * rate / per_units (integer math, rounds down)
            (amount as u128 * *rate_millicents as u128 / *per_units as u128) as u64
        }
        PricingRule::Tiered { tiers } => {
            let mut remaining = amount;
            let mut cost: u64 = 0;
            let mut prev_boundary: u64 = 0;

            for tier in tiers {
                if remaining == 0 { break; }
                let tier_capacity = match tier.up_to {
                    Some(up_to) => up_to.saturating_sub(prev_boundary),
                    None => remaining, // final tier covers the rest
                };
                let consumed = remaining.min(tier_capacity);
                if tier.per_units > 0 {
                    cost = cost.saturating_add(
                        (consumed as u128 * tier.rate_millicents as u128 / tier.per_units as u128) as u64,
                    );
                }
                remaining -= consumed;
                if let Some(up_to) = tier.up_to {
                    prev_boundary = up_to;
                }
            }
            cost
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_pricing() {
        let mut table = PricingTable::new();
        table.add_flat("requests", 300, 1_000_000); // $0.30/million
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 3_500_000);
        let cost = table.compute_cost(&usage);
        // 3.5M * 300 / 1M = 1050 millicents = $0.0105
        assert_eq!(cost, 1050);
    }

    #[test]
    fn multi_dimensional() {
        let table = PricingTable::cloudflare_comparable();
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 3_500_000);
        usage.insert("cpu_ms".to_string(), 17_500_000); // 17.5M ms
        usage.insert("egress_bytes".to_string(), 7_000_000_000); // 7 GB
        let cost = table.compute_cost(&usage);
        // requests: 3.5M * 300 / 1M = 1050
        // cpu: 17.5M * 12500 / 1M = 218750
        // egress: 7B * 90 / 1B = 630
        // total = 220430 millicents = $2.20
        assert_eq!(cost, 1050 + 218750 + 630);
    }

    #[test]
    fn tiered_pricing() {
        let mut table = PricingTable::new();
        table.add_tiered("requests", vec![
            PricingTier { up_to: Some(1_000_000), rate_millicents: 0, per_units: 1_000_000 },  // first 1M free
            PricingTier { up_to: Some(10_000_000), rate_millicents: 300, per_units: 1_000_000 }, // next 9M at $0.30/M
            PricingTier { up_to: None, rate_millicents: 200, per_units: 1_000_000 },  // remainder at $0.20/M
        ]);
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 15_000_000); // 15M
        let cost = table.compute_cost(&usage);
        // Tier 1: 1M free = 0
        // Tier 2: 9M * 300/1M = 2700
        // Tier 3: 5M * 200/1M = 1000
        // Total: 3700 millicents = $0.037
        assert_eq!(cost, 3700);
    }

    #[test]
    fn unknown_resource_is_free() {
        let table = PricingTable::new();
        let mut usage = HashMap::new();
        usage.insert("unknown".to_string(), 999999);
        assert_eq!(table.compute_cost(&usage), 0);
    }

    #[test]
    fn zero_usage_zero_cost() {
        let table = PricingTable::cloudflare_comparable();
        let usage = HashMap::new();
        assert_eq!(table.compute_cost(&usage), 0);
    }

    #[test]
    fn breakdown() {
        let table = PricingTable::cloudflare_comparable();
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 1_000_000);
        usage.insert("egress_bytes".to_string(), 1_000_000_000);
        let bd = table.compute_breakdown(&usage);
        assert_eq!(bd["requests"], 300);
        assert_eq!(bd["egress_bytes"], 90);
    }
}
