//! Every source of Control's PostgreSQL sessions, by the name it announces.
//!
//! Control holds `1 + K + H` sessions however many threads it serves on: one
//! shared multiplexed session, `K = control.catalog_max_connections` catalog
//! sessions and `H = control.retention_max_connections` retention sessions,
//! each opened at boot and kept. Request connections come on top of them, one
//! per `Registry::conn` call in flight, and close with the call.
//!
//! Each source announces its own name to `pg_stat_activity`, so an operator
//! and a test can attribute every session to the source that opened it.

/// The multiplexed autocommit session every thread shares (`control_pg`).
pub const SHARED: &str = "zeroship-control";
/// A session of the publication catalog (`crate::publication::Catalog`).
pub const CATALOG: &str = "zeroship-control-catalog";
/// A session of the retention executor: deployment holds and the deployment
/// collector.
pub const RETENTION: &str = "zeroship-control-retention";
/// A connection `Registry::conn` opens for one call.
pub const REQUEST: &str = "zeroship-control-request";

/// `url` offering `name` as its session name.
///
/// The name is offered as `fallback_application_name`, which PostgreSQL uses
/// only when nothing sets `application_name`. A URL that names a session of
/// its own, as either parameter, is left unchanged and every source then
/// announces the operator's name instead. So is a URL that does not parse, such
/// as a `key=value` connection string. The original text is kept, so its
/// encoding reaches the driver unchanged.
#[must_use]
pub fn named(url: &str, name: &str) -> String {
    const KEYS: [&str; 2] = ["application_name", "fallback_application_name"];
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    if parsed.fragment().is_some() || parsed.query_pairs().any(|(key, _)| KEYS.contains(&&*key)) {
        return url.to_owned();
    }
    let separator = match parsed.query() {
        None => "?",
        Some("") => "",
        Some(_) => "&",
    };
    format!("{url}{separator}fallback_application_name={name}")
}

#[cfg(test)]
mod tests {
    use super::{named, RETENTION};

    #[test]
    fn sessions_offer_their_name_without_displacing_a_configured_one() {
        let offered = format!("fallback_application_name={RETENTION}");
        for (url, expected) in [
            (
                "postgres://control@db:5432/zeroship",
                format!("postgres://control@db:5432/zeroship?{offered}"),
            ),
            (
                "postgresql://control@db/zeroship?sslmode=require",
                format!("postgresql://control@db/zeroship?sslmode=require&{offered}"),
            ),
            (
                "postgres://control@db/zeroship?",
                format!("postgres://control@db/zeroship?{offered}"),
            ),
        ] {
            assert_eq!(named(url, RETENTION), expected, "{url}");
        }
        for configured in [
            "postgres://control@db/zeroship?application_name=operator",
            "postgres://control@db/zeroship?fallback_application_name=operator",
            "postgres://control@db/zeroship?sslmode=require&application_name=operator",
            "host=db dbname=zeroship user=control",
            "not a url",
        ] {
            assert_eq!(named(configured, RETENTION), configured);
        }
    }
}
