//! Temporal wire values at `PostgreSQL`'s representational boundaries.
//!
//! `PostgreSQL`'s binary TIME is signed microseconds since midnight and admits
//! exactly 24:00:00 as a value distinct from 00:00:00. Its binary timestamps
//! are signed microseconds since 2000-01-01, with the two `i64` endpoints
//! reserved for infinity. These tests use the live server both to produce the
//! bytes and to witness what a rebound value means.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use compio_postgres::Client;

#[allow(unused_imports)]
use crate::common;

const POSTGRES_EPOCH_FROM_UNIX_SECS: u64 = 946_684_800;

#[allow(clippy::future_not_send)]
async fn connect_client() -> Client {
    let url = common::test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

fn postgres_epoch() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS)
}

/// `chrono` cannot represent `PostgreSQL`'s distinct end-of-day value, so direct
/// decoding must refuse it instead of silently returning midnight.
#[cfg(feature = "with-chrono-0_4")]
#[compio::test]
async fn chrono_does_not_alias_postgres_end_of_day_to_midnight() {
    let client = connect_client().await;
    let row = client
        .query_one(
            "SELECT '24:00:00'::time AS end_of_day, '00:00:00'::time AS midnight",
            &[],
        )
        .await
        .expect("PostgreSQL accepts both TIME values");

    let error = row
        .try_get::<_, chrono::NaiveTime>("end_of_day")
        .expect_err("24:00 must not silently decode as chrono midnight");
    let rendered = common::error_chain(&error);
    assert!(
        rendered.contains("column 0")
            && rendered.contains("24:00")
            && rendered.contains("chrono::NaiveTime"),
        "the refusal must name its column, wire value, and Rust target: {rendered}"
    );
    assert_eq!(
        row.get::<_, chrono::NaiveTime>("midnight"),
        chrono::NaiveTime::MIN,
        "the representable control value must still decode"
    );
}

/// The driver exposes an explicit carrier for applications that need to
/// preserve and rebound `PostgreSQL`'s legal 24:00 value losslessly.
#[cfg(feature = "with-chrono-0_4")]
#[compio::test]
async fn time_of_day_round_trips_postgres_end_of_day() {
    use compio_postgres::types::TimeOfDay;

    let client = connect_client().await;
    let row = client
        .query_one("SELECT '24:00:00'::time", &[])
        .await
        .expect("PostgreSQL emits its end-of-day TIME value");
    let value: TimeOfDay<chrono::NaiveTime> = row.get(0);
    assert_eq!(value, TimeOfDay::EndOfDay);

    let row = client
        .query_one(
            "SELECT $1::time = '24:00:00'::time, encode(time_send($1::time), 'hex')",
            &[&value],
        )
        .await
        .expect("the lossless carrier must bind as PostgreSQL TIME");
    assert!(row.get::<_, bool>(0));
    assert_eq!(
        row.get::<_, String>(1),
        "000000141dd76000",
        "TIME 24:00 is the signed i64 value 86,400,000,000"
    );
}

/// `time::Time` has the same representational limit as `chrono` and must make
/// the same refusal rather than changing a legal server value.
#[cfg(feature = "with-time-0_3")]
#[compio::test]
async fn time_crate_does_not_alias_postgres_end_of_day_to_midnight() {
    let client = connect_client().await;
    let row = client
        .query_one(
            "SELECT '24:00:00'::time AS end_of_day, '00:00:00'::time AS midnight",
            &[],
        )
        .await
        .expect("PostgreSQL accepts both TIME values");

    let error = row
        .try_get::<_, time::Time>("end_of_day")
        .expect_err("24:00 must not silently decode as time::Time midnight");
    let rendered = common::error_chain(&error);
    assert!(
        rendered.contains("column 0")
            && rendered.contains("24:00")
            && rendered.contains("time::Time"),
        "the refusal must name its column, wire value, and Rust target: {rendered}"
    );
    assert_eq!(
        row.get::<_, time::Time>("midnight"),
        time::Time::MIDNIGHT,
        "the representable control value must still decode"
    );
}

/// A duration that fits in `u64` microseconds but not `i64` used to wrap
/// through `as i64`, turning a remote future into one second before 2000.
#[compio::test]
async fn system_time_larger_than_the_timestamp_wire_cannot_wrap() {
    let client = connect_client().await;
    let value = postgres_epoch()
        .checked_add(Duration::from_micros(u64::MAX - 999_999))
        .expect("the Linux SystemTime range contains the probe");

    let error = client
        .query_one("SELECT $1::timestamp", &[&value])
        .await
        .expect_err("an unrepresentable SystemTime must fail before it can wrap");
    assert!(
        common::error_chain(&error).contains("too large"),
        "the serialization error must explain the range failure: {}",
        common::error_chain(&error)
    );

    let recovered: i32 = client
        .query_one_scalar("SELECT 41::int4", &[])
        .await
        .expect("a local serialization refusal must leave the connection usable");
    assert_eq!(recovered, 41);
}

/// The negative endpoint used to narrow to `i64::MIN` and then panic while
/// negating it. A value outside `PostgreSQL`'s finite range is an error, not a
/// process abort.
#[compio::test]
async fn system_time_at_the_negative_wire_endpoint_cannot_panic() {
    let client = connect_client().await;
    let value = postgres_epoch()
        .checked_sub(Duration::from_micros(1_u64 << 63))
        .expect("the Linux SystemTime range contains the probe");

    let error = client
        .query_one("SELECT $1::timestamp", &[&value])
        .await
        .expect_err("a finite SystemTime must not alias -infinity or panic");
    assert!(
        common::error_chain(&error).contains("too large"),
        "the serialization error must explain the range failure: {}",
        common::error_chain(&error)
    );
}

/// Bare `SystemTime` has no infinity variant. The generic `Timestamp` wrapper is
/// the lossless target; decoding a sentinel into bare `SystemTime` must error.
#[compio::test]
async fn bare_system_time_refuses_timestamp_infinity() {
    use compio_postgres::types::Timestamp;

    let client = connect_client().await;
    let row = client
        .query_one(
            "SELECT 'infinity'::timestamp AS positive, '-infinity'::timestamp AS negative",
            &[],
        )
        .await
        .expect("PostgreSQL emits both timestamp sentinels");

    row.try_get::<_, SystemTime>("positive")
        .expect_err("bare SystemTime must not turn infinity into a finite instant");
    row.try_get::<_, SystemTime>("negative")
        .expect_err("bare SystemTime must not turn -infinity into a finite instant");

    assert_eq!(
        row.get::<_, Timestamp<SystemTime>>("positive"),
        Timestamp::PosInfinity
    );
    assert_eq!(
        row.get::<_, Timestamp<SystemTime>>("negative"),
        Timestamp::NegInfinity
    );

    let positive = Timestamp::<SystemTime>::PosInfinity;
    let negative = Timestamp::<SystemTime>::NegInfinity;
    let row = client
        .query_one(
            "SELECT $1::timestamp = 'infinity'::timestamp, \
                    $2::timestamp = '-infinity'::timestamp, \
                    encode(timestamp_send($1::timestamp), 'hex'), \
                    encode(timestamp_send($2::timestamp), 'hex')",
            &[&positive, &negative],
        )
        .await
        .expect("the explicit infinity carrier must bind both sentinels");
    assert!(row.get::<_, bool>(0));
    assert!(row.get::<_, bool>(1));
    assert_eq!(row.get::<_, String>(2), "7fffffffffffffff");
    assert_eq!(row.get::<_, String>(3), "8000000000000000");
    assert_eq!(
        row.columns()[2].type_(),
        &compio_postgres::types::Type::TEXT,
        "encode(timestamp_send(...), 'hex') must expose the witnessed bytes"
    );
}
