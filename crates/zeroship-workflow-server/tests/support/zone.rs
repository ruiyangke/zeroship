use std::rc::Rc;
use zeroship_workflow_manager::eligibility::{EligibilitySource, LocalEligibility, ZoneId};

/// Trusted single-zone facts for contracts that do not exercise eligibility:
/// every app and worker is in the seeded zone and active.
pub fn trusted() -> Rc<dyn EligibilitySource> {
    Rc::new(LocalEligibility::new(ZoneId::default_zone()))
}
