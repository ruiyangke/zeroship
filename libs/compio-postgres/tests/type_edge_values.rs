//! Edge values must survive a round trip through the SERVER unchanged.
//!
//! Every `Bind` this driver sends asks for binary results, so these values are
//! encoded and decoded by this crate's own `ToSql`/`FromSql` rather than by
//! PostgreSQL's text formatter. That makes the failure mode a WRONG VALUE
//! rather than an error, which is the worst kind: nothing raises, and the
//! caller acts on a number the database does not hold.
//!
//! The existing `numeric_types` test in `integration.rs` covers only ordinary
//! values (-123, 42000, 3.14). None of the cases below -- NaN, the infinities,
//! negative zero, the type extremes -- appear anywhere in the suite.
//!
//! Floats are compared BY BITS, not by `==`. `NaN == NaN` is false, so an `==`
//! assertion on NaN passes vacuously no matter what came back, and `0.0 ==
//! -0.0` is true, so an `==` assertion cannot see a lost sign bit at all. Bit
//! equality is the only comparison that can fail for the right reason here.

use compio_postgres::{Client, NoTls};

mod common;

fn test_url() -> Option<String> {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// `f64` edge values round-trip bit-for-bit.
#[compio::test]
async fn every_f64_edge_value_survives_the_server_unchanged() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;

    let cases: &[(&str, f64)] = &[
        ("NaN", f64::NAN),
        ("+inf", f64::INFINITY),
        ("-inf", f64::NEG_INFINITY),
        ("negative zero", -0.0),
        ("positive zero", 0.0),
        ("MIN", f64::MIN),
        ("MAX", f64::MAX),
        ("MIN_POSITIVE", f64::MIN_POSITIVE),
        ("EPSILON", f64::EPSILON),
    ];

    for (label, value) in cases {
        let row = client
            .query_one("SELECT $1::float8", &[value])
            .await
            .unwrap_or_else(|error| panic!("{label}: round trip failed: {error}"));
        let returned: f64 = row.get(0);

        assert_eq!(
            returned.to_bits(),
            value.to_bits(),
            "{label}: float8 came back as a different value ({returned} vs {value})"
        );
    }
}

/// The same for `f32`, which has its own encode path.
#[compio::test]
async fn every_f32_edge_value_survives_the_server_unchanged() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;

    let cases: &[(&str, f32)] = &[
        ("NaN", f32::NAN),
        ("+inf", f32::INFINITY),
        ("-inf", f32::NEG_INFINITY),
        ("negative zero", -0.0),
        ("MIN", f32::MIN),
        ("MAX", f32::MAX),
        ("MIN_POSITIVE", f32::MIN_POSITIVE),
        ("EPSILON", f32::EPSILON),
    ];

    for (label, value) in cases {
        let row = client
            .query_one("SELECT $1::float4", &[value])
            .await
            .unwrap_or_else(|error| panic!("{label}: round trip failed: {error}"));
        let returned: f32 = row.get(0);

        assert_eq!(
            returned.to_bits(),
            value.to_bits(),
            "{label}: float4 came back as a different value ({returned} vs {value})"
        );
    }
}

/// Negative zero is worth its own test because the SERVER is the thing that
/// could lose it, and `==` cannot see the loss.
///
/// The assertion is made twice from different directions: the bits must differ
/// from positive zero's, and PostgreSQL must independently agree the value is
/// negative by rendering it as `-0`. If only the Rust side were checked, a
/// driver that never sent the value at all and echoed the parameter back would
/// pass.
#[compio::test]
async fn negative_zero_keeps_its_sign_through_the_server() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;

    let row = client
        .query_one("SELECT $1::float8, ($1::float8)::text", &[&-0.0f64])
        .await
        .expect("round trip negative zero");

    let returned: f64 = row.get(0);
    let rendered: String = row.get(1);

    assert_eq!(
        returned.to_bits(),
        (-0.0f64).to_bits(),
        "negative zero lost its sign bit (came back as {returned})"
    );
    assert_ne!(
        returned.to_bits(),
        0.0f64.to_bits(),
        "negative zero became positive zero"
    );
    assert_eq!(
        rendered, "-0",
        "the SERVER did not agree the value was negative zero"
    );
}

/// Integer extremes round-trip.
#[compio::test]
async fn integer_extremes_survive_the_server_unchanged() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;

    for value in [i16::MIN, i16::MAX, 0, -1] {
        let row = client
            .query_one("SELECT $1::int2", &[&value])
            .await
            .unwrap_or_else(|error| panic!("int2 {value}: {error}"));
        assert_eq!(row.get::<_, i16>(0), value, "int2 {value} changed");
    }

    for value in [i32::MIN, i32::MAX, 0, -1] {
        let row = client
            .query_one("SELECT $1::int4", &[&value])
            .await
            .unwrap_or_else(|error| panic!("int4 {value}: {error}"));
        assert_eq!(row.get::<_, i32>(0), value, "int4 {value} changed");
    }

    for value in [i64::MIN, i64::MAX, 0, -1] {
        let row = client
            .query_one("SELECT $1::int8", &[&value])
            .await
            .unwrap_or_else(|error| panic!("int8 {value}: {error}"));
        assert_eq!(row.get::<_, i64>(0), value, "int8 {value} changed");
    }
}

/// Every byte value survives a `bytea` round trip, including NUL.
///
/// NUL is the interesting one: it terminates a C string, so any path that
/// treated a value as text would truncate here and the failure would show up
/// as a SHORT result rather than an error.
#[compio::test]
async fn every_byte_value_survives_a_bytea_round_trip() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;

    let all_bytes: Vec<u8> = (0..=255u8).collect();
    let row = client
        .query_one("SELECT $1::bytea", &[&all_bytes])
        .await
        .expect("round trip every byte value");
    let returned: Vec<u8> = row.get(0);

    assert_eq!(returned.len(), 256, "bytea came back a different length");
    assert_eq!(returned, all_bytes, "bytea round trip altered a byte");

    // An empty bytea is not a NULL bytea.
    let row = client
        .query_one("SELECT $1::bytea", &[&Vec::<u8>::new()])
        .await
        .expect("round trip an empty bytea");
    assert_eq!(
        row.get::<_, Vec<u8>>(0),
        Vec::<u8>::new(),
        "an empty bytea did not come back empty"
    );
}
