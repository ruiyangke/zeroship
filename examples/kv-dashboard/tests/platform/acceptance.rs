//! Own the demo's platform; Vitest and Playwright assert its public behavior.
mod issuer;
mod platform;

#[test]
fn dashboard_agrees_in_dev_and_deployed() {
    let mut platform = platform::Platform::start();
    platform.test_example();
    platform.assert_alive();
}
