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

use compio_postgres::Client;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
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
    let url = test_url();
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
    let url = test_url();
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
    let url = test_url();
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
    let url = test_url();
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
    let url = test_url();
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

/// Arrays: NULL elements, empty, and NULL-vs-empty are three distinct things.
///
/// The element type is decoded per value, so a NULL element has to reach
/// `Option::None` rather than collapsing into a default. An empty array and a
/// NULL array are also different values with the same "nothing here" feel, and
/// a driver that conflated them would look right in casual use.
#[compio::test]
async fn array_null_elements_and_emptiness_are_distinguished() {
    let url = test_url();
    let client = connect_client(&url).await;

    // A NULL in the middle, so an off-by-one in element walking shows up as a
    // shifted value rather than a length change.
    let row = client
        .query_one("SELECT ARRAY[1, NULL, 3]::int4[]", &[])
        .await
        .expect("array with a NULL element");
    assert_eq!(
        row.get::<_, Vec<Option<i32>>>(0),
        vec![Some(1), None, Some(3)],
        "a NULL array element did not decode as None in place"
    );

    // Empty array: present, zero elements.
    let row = client
        .query_one("SELECT ARRAY[]::int4[]", &[])
        .await
        .expect("empty array");
    assert_eq!(
        row.get::<_, Vec<i32>>(0),
        Vec::<i32>::new(),
        "an empty array did not decode as an empty Vec"
    );

    // NULL array: absent entirely. Distinct from the empty array above.
    let row = client
        .query_one("SELECT NULL::int4[]", &[])
        .await
        .expect("null array");
    assert_eq!(
        row.get::<_, Option<Vec<i32>>>(0),
        None,
        "a NULL array must be None, not an empty Vec"
    );

    // And the discriminating pair: the empty array must NOT read as None.
    let row = client
        .query_one("SELECT ARRAY[]::int4[]", &[])
        .await
        .expect("empty array again");
    assert_eq!(
        row.get::<_, Option<Vec<i32>>>(0),
        Some(Vec::new()),
        "an empty array collapsed into NULL"
    );
}

/// A two-dimensional array is REFUSED, not silently flattened.
///
/// PostgreSQL arrays carry their dimension count on the wire and `Vec<T>` can
/// only represent one dimension. Flattening `{{1,2},{3,4}}` to `[1,2,3,4]`
/// would hand the caller four values where the database holds a 2x2 -- a wrong
/// answer with no error, which is the failure mode this suite exists to catch.
///
/// This also pins that the refusal arrives as an `Err` through this driver's
/// own error path. The decoder it delegates to `panic!`s outright on a
/// mismatched type kind, so "returns an error" is a claim about our stack, not
/// only about the decoder.
#[compio::test]
async fn a_multidimensional_array_is_refused_rather_than_flattened() {
    let url = test_url();
    let client = connect_client(&url).await;

    let row = client
        .query_one("SELECT ARRAY[[1, 2], [3, 4]]::int4[]", &[])
        .await
        .expect("the QUERY itself is valid; only the decode should object");

    let error = row
        .try_get::<_, Vec<i32>>(0)
        .expect_err("a 2-D array must not decode into a 1-D Vec");
    let rendered = format!("{error}");
    let chain = std::iter::successors(std::error::Error::source(&error), |error| {
        std::error::Error::source(*error)
    })
    .map(|cause| cause.to_string())
    .collect::<Vec<_>>()
    .join("; ");
    assert!(
        chain.contains("dimensions") || rendered.contains("dimensions"),
        "the refusal should say what was wrong: {rendered} / {chain}"
    );

    // Control, one variable away: the SAME query shape in one dimension must
    // decode, so the test above cannot be satisfied by refusing every array.
    let row = client
        .query_one("SELECT ARRAY[1, 2, 3, 4]::int4[]", &[])
        .await
        .expect("one-dimensional array");
    assert_eq!(row.get::<_, Vec<i32>>(0), vec![1, 2, 3, 4]);
}

/// A width mismatch is REFUSED, never silently narrowed or widened.
///
/// This is the same silent-wrong-value family as the rest of this file, and
/// the narrowing direction is the dangerous one: an `int8` of 9_000_000_000
/// does not fit an `i32`, so a driver that truncated would hand back a number
/// the database does not hold, with no error. The widening direction is
/// checked too, because a decoder loose enough to widen is loose enough to
/// narrow -- they are the same missing gate.
///
/// The refusal is `accepts`-driven, so it happens before any bytes are
/// interpreted; the assertions below are about that gate being wired into this
/// driver's own accessor, not about the decoder in isolation.
#[compio::test]
async fn an_integer_width_mismatch_is_refused_not_truncated() {
    let url = test_url();
    let client = connect_client(&url).await;

    // Narrowing: the value genuinely does not fit.
    let row = client
        .query_one("SELECT 9000000000::int8", &[])
        .await
        .expect("int8 out of i32 range");
    row.try_get::<_, i32>(0)
        .expect_err("an int8 must not be truncated into an i32");

    // Narrowing where the value WOULD fit: still refused, because the gate is
    // on the TYPE, not on the value. If this passed while the case above
    // failed, the driver would be range-checking instead of type-checking --
    // silently correct until the day a value grew.
    let row = client
        .query_one("SELECT 1::int8", &[])
        .await
        .expect("small int8");
    row.try_get::<_, i32>(0)
        .expect_err("an int8 that happens to fit an i32 is still an int8");

    // Widening is refused as well.
    let row = client.query_one("SELECT 1::int4", &[]).await.expect("int4");
    row.try_get::<_, i64>(0)
        .expect_err("an int4 must not be widened into an i64");

    // Control, one variable away: the MATCHING width must decode, so none of
    // the above can be satisfied by refusing every integer.
    let row = client
        .query_one("SELECT 9000000000::int8", &[])
        .await
        .expect("int8 again");
    assert_eq!(row.get::<_, i64>(0), 9_000_000_000i64);
}

/// Reading a text column as a number is refused, and vice versa.
#[compio::test]
async fn a_type_mismatch_across_families_is_refused() {
    let url = test_url();
    let client = connect_client(&url).await;

    let row = client
        .query_one("SELECT '42'::text", &[])
        .await
        .expect("text row");
    row.try_get::<_, i32>(0)
        .expect_err("text that looks numeric is still text");
    assert_eq!(
        row.get::<_, String>(0),
        "42",
        "the control: text reads as text"
    );

    let row = client
        .query_one("SELECT 42::int4", &[])
        .await
        .expect("int row");
    row.try_get::<_, String>(0)
        .expect_err("an int4 must not be rendered into a String by the decoder");
    assert_eq!(row.get::<_, i32>(0), 42, "the control: int4 reads as int4");
}
