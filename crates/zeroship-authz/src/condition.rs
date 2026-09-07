use serde::{Deserialize, Serialize};

/// A constraint a wrapper policy statement may add on top of its action and
/// resource scope.
///
/// **`RequireMfa` and `MfaWithin` were DELETED, not renamed.** They lowered to
/// `context.mfa_verified == true` and `context.mfa_age_seconds <= N`, and every
/// producer of a live `zeroship_authn::VerifiedPrincipal` set `mfa_verified:
/// false` / `mfa_age_seconds: None` unconditionally, because the platform has
/// no second-factor signal to report. So the comparison was not against an
/// unknown value, it was against a WRONG one: either condition could only ever
/// evaluate false, and a statement carrying one could only ever deny.
///
/// A condition that silently never matches is worse than an absent one - it
/// reads to a reviewer as a fence that is enforced. The two variants, the two
/// `AuthzContext` fields, the two context keys and the two `VerifiedPrincipal`
/// fields went together, so nothing is left to reconstruct them from.
///
/// The two that remain read live inputs: `request_ip` comes from the edge and
/// `now_minute_utc` from the clock, so both can be true.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    IpRange {
        cidrs: Vec<String>,
    },
    TimeWindow {
        start: String,
        end: String,
        tz: String,
    },
}
