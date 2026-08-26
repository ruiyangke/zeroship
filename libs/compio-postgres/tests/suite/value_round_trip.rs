//! Does a value survive a round trip unchanged?
//!
//! The rest of the suite rules on command tags, SQLSTATEs, error fields and
//! notices - what the server SAYS. This file rules on what it gives back.
//!
//! The cases are the boundaries, because that is where an encoding is wrong
//! without being obviously wrong: a float's negative zero (whose sign a text
//! encoding drops), NaN (which is not equal to itself, so a careless test
//! passes no matter what came back), the infinities, and the exact minimum and
//! maximum of each integer width, where a sign error or an off-by-one is
//! invisible everywhere else.
//!
//! Floats are compared as BITS. Comparing them as numbers would make the NaN
//! case pass for any returned NaN payload and, worse, make a returned `0.0`
//! indistinguishable from the `-0.0` that was sent.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_url};
use compio_postgres::Client;
use compio_postgres::types::ToSql;

async fn connected() -> Client {
    let (client, connection) = compio_postgres::connect(&test_url(), suite_tls())
        .await
        .expect("connect to the test server");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// Send `value` as a bound parameter and read it straight back.
///
/// `SELECT $1` alone has no type to infer, so each case names the type it is
/// exercising. That also pins WHICH server type the mapping goes through
/// rather than letting the server pick one.
async fn round_trip<T>(client: &Client, sql_type: &str, value: T) -> T
where
    T: for<'a> compio_postgres::types::FromSqlOwned + ToSql + Sync,
{
    client
        .query_one_scalar(&format!("SELECT $1::{sql_type}"), &[&value])
        .await
        .expect("the round trip completes")
}

#[compio::test]
async fn float8_special_values_survive_bit_exactly() {
    let client = connected().await;

    for (name, sent) in [
        ("negative zero", -0.0f64),
        ("positive zero", 0.0f64),
        ("infinity", f64::INFINITY),
        ("negative infinity", f64::NEG_INFINITY),
        ("min", f64::MIN),
        ("max", f64::MAX),
        ("smallest positive", f64::MIN_POSITIVE),
        ("epsilon", f64::EPSILON),
    ] {
        let got: f64 = round_trip(&client, "float8", sent).await;
        assert_eq!(
            got.to_bits(),
            sent.to_bits(),
            "float8 {name} came back as a different value: sent {sent:?} ({:#x}), got {got:?} ({:#x})",
            sent.to_bits(),
            got.to_bits()
        );
    }

    // NaN separately: it is never equal to itself, so the bit comparison is
    // the only one that means anything here.
    let got: f64 = round_trip(&client, "float8", f64::NAN).await;
    assert!(got.is_nan(), "float8 NaN came back as {got:?}");
}

#[compio::test]
async fn float4_special_values_survive_bit_exactly() {
    let client = connected().await;

    for (name, sent) in [
        ("negative zero", -0.0f32),
        ("infinity", f32::INFINITY),
        ("negative infinity", f32::NEG_INFINITY),
        ("min", f32::MIN),
        ("max", f32::MAX),
        ("smallest positive", f32::MIN_POSITIVE),
    ] {
        let got: f32 = round_trip(&client, "float4", sent).await;
        assert_eq!(
            got.to_bits(),
            sent.to_bits(),
            "float4 {name} came back as a different value: sent {sent:?}, got {got:?}"
        );
    }

    let got: f32 = round_trip(&client, "float4", f32::NAN).await;
    assert!(got.is_nan(), "float4 NaN came back as {got:?}");
}

/// THE CONTROL for both float cases. If the comparison above could not fail,
/// every assertion in this file is decoration. Sending one value and asserting
/// it equals a DIFFERENT one must fail, and `-0.0` vs `0.0` is exactly the
/// pair a numeric comparison would wave through.
#[compio::test]
async fn the_bit_comparison_can_actually_fail() {
    let client = connected().await;

    let negative_zero: f64 = round_trip(&client, "float8", -0.0f64).await;
    assert_ne!(
        negative_zero.to_bits(),
        0.0f64.to_bits(),
        "negative zero and positive zero have the same bits, so this file's \
         float assertions cannot distinguish them"
    );
    // And they ARE numerically equal, which is why bits are compared.
    assert!(negative_zero == 0.0f64);
}

#[compio::test]
async fn integer_extremes_survive() {
    let client = connected().await;

    for (sql_type, sent) in [("int2", i16::MIN), ("int2", i16::MAX)] {
        let got: i16 = round_trip(&client, sql_type, sent).await;
        assert_eq!(got, sent, "{sql_type} {sent} did not round trip");
    }
    for sent in [i32::MIN, i32::MAX, -1, 0] {
        let got: i32 = round_trip(&client, "int4", sent).await;
        assert_eq!(got, sent, "int4 {sent} did not round trip");
    }
    for sent in [i64::MIN, i64::MAX, -1, 0] {
        let got: i64 = round_trip(&client, "int8", sent).await;
        assert_eq!(got, sent, "int8 {sent} did not round trip");
    }
}

/// `bytea` has to be byte-transparent. A NUL is the byte most likely to
/// terminate something that should not be terminated, and 0xFF is the one most
/// likely to be mangled by a text encoding that crept into the path.
#[compio::test]
async fn bytea_is_byte_transparent() {
    let client = connected().await;

    let cases: [(&str, Vec<u8>); 5] = [
        ("empty", vec![]),
        ("a single NUL", vec![0]),
        ("NULs around data", vec![0, b'a', 0, b'b', 0]),
        ("every high bit set", vec![0xFF; 8]),
        ("all 256 byte values", (0u8..=255).collect()),
    ];

    for (name, sent) in cases {
        let got: Vec<u8> = round_trip(&client, "bytea", sent.clone()).await;
        assert_eq!(got, sent, "bytea with {name} did not round trip");
    }
}

/// Text has to survive a NUL-free but otherwise awkward payload. PostgreSQL
/// rejects a NUL inside `text` itself, so that is not a driver question; what
/// is, is whether multi-byte sequences and a trailing newline come back whole.
#[compio::test]
async fn text_survives_multibyte_and_whitespace() {
    let client = connected().await;

    for (name, sent) in [
        ("empty", ""),
        ("a trailing newline", "line\n"),
        ("a lone carriage return", "\r"),
        ("four-byte code points", "\u{1F600}\u{1F680}"),
        ("a backslash and a quote", "\\'\""),
    ] {
        let got: String = round_trip(&client, "text", sent.to_owned()).await;
        assert_eq!(got, sent, "text with {name} did not round trip");
    }
}
