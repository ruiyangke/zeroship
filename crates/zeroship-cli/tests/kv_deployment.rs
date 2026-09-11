//! The KV dashboard contract through local Vite and the deployed platform.
//! All backing servers belong to Testcontainers; no external database is used.
//! Run the workspace's `pnpm install` and `pnpm build` prerequisites before Cargo,
//! as required by the runtime's embedded SDK modules.

#[path = "kv_support/contract.rs"]
mod contract;
#[path = "kv_support/platform.rs"]
mod platform;

#[test]
fn kv_dashboard_agrees_in_dev_and_deployed() {
    let mut platform = platform::Platform::start();
    let dev = contract::exercise(&platform.http, &platform.dev_url);
    contract::validate(&dev).expect("local KV contract");
    let deployed = contract::exercise(&platform.http, &platform.deployed_url);
    contract::validate(&deployed).expect("deployed KV contract");
    contract::assert_oracle_controls(&deployed);
    assert_eq!(contract::normalize(dev), contract::normalize(deployed));
    platform.assert_alive();
}
