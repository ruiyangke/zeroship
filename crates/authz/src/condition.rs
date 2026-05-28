use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    IpRange { cidrs: Vec<String> },
    TimeWindow { start: String, end: String, tz: String },
    RequireMfa,
    MfaWithin { seconds: u32 },
}
