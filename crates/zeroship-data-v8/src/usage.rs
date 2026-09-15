//! Attribute a creator binding's database usage to its app.
//!
//! The ORM reports usage to whatever sink a dispatch carries and interprets no
//! identity itself. This adapter serves creator apps, so it builds that sink
//! from the service's meter and the binding's app id, and it is where an app
//! id that cannot be attributed is refused.

use std::sync::Arc;

use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::metrics::UsageSink;

/// Records one app's ORM usage into the process meter.
#[derive(Debug)]
struct AppUsage(zeroship_metering::MeterHandle);

impl UsageSink for AppUsage {
    fn record(&self, metric: &str, amount: u64) {
        self.0.record(metric, amount);
    }
}

/// The sink a creator dispatch for `binding` reports its usage to.
///
/// `None` when the service meters nothing. Otherwise the binding must belong
/// to an app: a metered binding whose app id is not one is refused rather than
/// served without attribution.
pub(crate) fn sink_for(binding: &DbBinding) -> Result<Option<Arc<dyn UsageSink>>, DbError> {
    let Some(meter) = crate::context::with(|context| context.meter()) else {
        return Ok(None);
    };
    let app = zeroship_core::AppId::parse(binding.app_id())
        .map_err(|error| DbError::config("invalid_meter_app_id", error.to_string()))?;
    Ok(Some(Arc::new(AppUsage(
        zeroship_metering::MeterHandle::new(meter, app),
    ))))
}
