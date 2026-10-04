//! An operator-declared execution zone is a deployment-global fact.
//!
//! The shared working database seeds exactly the deployment's default zone. A
//! case that adds a second one and leaves it there refuses every sibling's
//! zero-config app creation, so a case whose subject declares a zone must work
//! in a clone no sibling observes.

use crate::support::{platform, zone};

#[compio::test]
#[should_panic(expected = "an operator-declared execution zone is deployment-global")]
async fn declaring_an_operator_zone_on_the_shared_database_is_refused() {
    let shared = platform::Platform::new().await;
    let _ = zone::declare_zone(&shared).await;
}
