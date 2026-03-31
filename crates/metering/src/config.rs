//! TOML config parsing for metering plans.
//!
//! Loads quota plans, default plan assignment, and per-app plan overrides
//! from an `appbase.toml` configuration file.

use crate::plan::{Period, QuotaDef, QuotaPlan, RateLimitDef};
use std::collections::HashMap;

/// Parsed metering configuration from TOML.
#[derive(Debug)]
pub struct MeteringConfig {
    /// Named quota plans (e.g. "free", "pro").
    pub plans: HashMap<String, QuotaPlan>,
    /// Default plan name for apps without an explicit assignment.
    pub default_plan: String,
    /// Per-app plan overrides: app_id -> plan name.
    pub app_plans: HashMap<String, String>,
}

impl MeteringConfig {
    /// Load and parse a TOML config file.
    pub fn load(path: &str) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read {path}: {e}"))?;
        let toml: toml::Value = content
            .parse()
            .map_err(|e| format!("Failed to parse TOML: {e}"))?;
        Self::from_toml(&toml)
    }

    fn from_toml(toml: &toml::Value) -> Result<Self, String> {
        // Parse plans
        let mut plans = HashMap::new();
        if let Some(plans_table) = toml.get("plans").and_then(|v| v.as_table()) {
            for (name, plan_val) in plans_table {
                let plan = parse_plan(name, plan_val)?;
                plans.insert(name.clone(), plan);
            }
        }

        // Parse defaults
        let default_plan = toml
            .get("defaults")
            .and_then(|v| v.get("plan"))
            .and_then(|v| v.as_str())
            .unwrap_or("free")
            .to_string();

        // Parse app assignments
        let mut app_plans = HashMap::new();
        if let Some(apps_table) = toml.get("apps").and_then(|v| v.as_table()) {
            for (app_id, app_val) in apps_table {
                if let Some(plan) = app_val.get("plan").and_then(|v| v.as_str()) {
                    app_plans.insert(app_id.clone(), plan.to_string());
                }
            }
        }

        Ok(Self {
            plans,
            default_plan,
            app_plans,
        })
    }

    /// Get the plan for a specific app (falls back to default plan).
    pub fn plan_for_app(&self, app_id: &str) -> QuotaPlan {
        let plan_name = self.app_plans.get(app_id).unwrap_or(&self.default_plan);
        self.plans
            .get(plan_name)
            .cloned()
            .unwrap_or_else(QuotaPlan::unlimited)
    }
}

fn parse_plan(name: &str, val: &toml::Value) -> Result<QuotaPlan, String> {
    let description = val
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut quotas = HashMap::new();
    if let Some(q_table) = val.get("quotas").and_then(|v| v.as_table()) {
        for (resource, q_val) in q_table {
            let max = q_val.get("max").and_then(|v| v.as_integer()).map(|v| v as u64);
            let period = match q_val
                .get("period")
                .and_then(|v| v.as_str())
                .unwrap_or("monthly")
            {
                "per_request" => Period::PerRequest,
                "daily" => Period::Daily,
                "monthly" => Period::Monthly,
                "absolute" => Period::Absolute,
                other => return Err(format!("Unknown period: {other}")),
            };
            let policy = q_val
                .get("policy")
                .and_then(|v| v.as_str())
                .unwrap_or("warn_then_block")
                .to_string();
            quotas.insert(resource.clone(), QuotaDef { max, period, policy });
        }
    }

    let mut rate_limits = HashMap::new();
    if let Some(rl_table) = val.get("rate_limits").and_then(|v| v.as_table()) {
        for (rl_name, rl_val) in rl_table {
            let max_per_second = rl_val
                .get("max_per_second")
                .and_then(|v| v.as_integer())
                .unwrap_or(10) as u32;
            let burst = rl_val
                .get("burst")
                .and_then(|v| v.as_integer())
                .unwrap_or(50) as u32;
            let policy = rl_val
                .get("policy")
                .and_then(|v| v.as_str())
                .unwrap_or("reject")
                .to_string();
            rate_limits.insert(
                rl_name.clone(),
                RateLimitDef {
                    max_per_second,
                    burst,
                    policy,
                },
            );
        }
    }

    Ok(QuotaPlan {
        name: name.to_string(),
        version: 1,
        description,
        entitlements: HashMap::new(),
        quotas,
        rate_limits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_example_config() {
        let config = MeteringConfig::load("../../examples/appbase.toml")
            .expect("Failed to load example config");

        assert_eq!(config.plans.len(), 2);
        assert!(config.plans.contains_key("free"));
        assert!(config.plans.contains_key("pro"));
        assert_eq!(config.default_plan, "free");
        assert_eq!(config.app_plans.get("default"), Some(&"pro".to_string()));
    }

    #[test]
    fn test_plan_for_app_with_override() {
        let config = MeteringConfig::load("../../examples/appbase.toml")
            .expect("Failed to load example config");

        // "default" app is assigned to "pro" plan
        let plan = config.plan_for_app("default");
        assert_eq!(plan.name, "pro");
        assert_eq!(plan.description, "Pro tier");
    }

    #[test]
    fn test_plan_for_app_falls_back_to_default() {
        let config = MeteringConfig::load("../../examples/appbase.toml")
            .expect("Failed to load example config");

        // Unknown app falls back to default plan ("free")
        let plan = config.plan_for_app("unknown-app");
        assert_eq!(plan.name, "free");
        assert_eq!(plan.description, "Free tier");
    }

    #[test]
    fn test_free_plan_quotas() {
        let config = MeteringConfig::load("../../examples/appbase.toml")
            .expect("Failed to load example config");

        let free = config.plans.get("free").unwrap();
        assert_eq!(free.quotas.len(), 7);
        assert_eq!(free.quotas.get("cpu_ms").unwrap().max, Some(50_000));
        assert_eq!(free.quotas.get("requests").unwrap().max, Some(100_000));
        assert_eq!(
            free.quotas.get("cpu_per_request").unwrap().policy,
            "hard_kill"
        );

        assert_eq!(free.rate_limits.len(), 1);
        assert_eq!(
            free.rate_limits.get("default").unwrap().max_per_second,
            10
        );
    }

    #[test]
    fn test_pro_plan_quotas() {
        let config = MeteringConfig::load("../../examples/appbase.toml")
            .expect("Failed to load example config");

        let pro = config.plans.get("pro").unwrap();
        assert_eq!(pro.quotas.len(), 3);
        assert_eq!(pro.quotas.get("cpu_ms").unwrap().max, Some(30_000_000));
        assert_eq!(
            pro.rate_limits.get("default").unwrap().max_per_second,
            1000
        );
    }

    #[test]
    fn test_from_toml_empty() {
        let toml: toml::Value = "".parse().unwrap();
        let config = MeteringConfig::from_toml(&toml).unwrap();
        assert_eq!(config.plans.len(), 0);
        assert_eq!(config.default_plan, "free");
        assert_eq!(config.app_plans.len(), 0);
    }

    #[test]
    fn test_unknown_period_returns_error() {
        let toml: toml::Value = r#"
            [plans.bad]
            description = "Bad plan"
            [plans.bad.quotas]
            cpu = { max = 100, period = "yearly", policy = "block" }
        "#
        .parse()
        .unwrap();

        let result = MeteringConfig::from_toml(&toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown period: yearly"));
    }
}
