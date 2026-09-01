//! Direct `ToSql` conformance against `PostgreSQL`'s own binary COPY output.
//!
//! Unlike the driver-vs-driver differential suite, this oracle does not share
//! `postgres-types` with the implementation under test. The outbound side is
//! the exact `BytesMut` filled by this driver's `ToSql`; the reference side is
//! extracted from `COPY (SELECT value) TO STDOUT WITH (FORMAT binary)`.
//!
//! `postgres-types` has no native NUMERIC, INTERVAL, range, multirange, money,
//! or composite carrier. Test-local carriers for those families validate the
//! public raw-parameter transport and their explicitly hand-written framing;
//! they are not described as production codecs.

use std::error::Error;
use std::fmt::Write as _;

use compio_postgres::types::{self, IsNull, ToSql, Type};
use futures_util::{SinkExt, TryStreamExt};

use crate::common;

type BoxError = Box<dyn Error + Send + Sync>;

const BINARY_COPY_SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

#[allow(clippy::future_not_send)]
async fn compio_client() -> compio_postgres::Client {
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

async fn binary_copy_payload(client: &compio_postgres::Client, expression: &str) -> Vec<u8> {
    let sql = format!("COPY (SELECT {expression}) TO STDOUT WITH (FORMAT binary)");
    let chunks: Vec<bytes::Bytes> = client
        .copy_out(&sql)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "binary COPY for {expression}: {}",
                common::error_chain(&error)
            )
        })
        .try_collect()
        .await
        .unwrap_or_else(|error| {
            panic!(
                "drain binary COPY for {expression}: {}",
                common::error_chain(&error)
            )
        });
    chunks.into_iter().flatten().collect()
}

struct CopyReader<'a> {
    remaining: &'a [u8],
}

impl<'a> CopyReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, len: usize) -> &'a [u8] {
        let value = self
            .remaining
            .get(..len)
            .unwrap_or_else(|| panic!("binary COPY payload ended {len} bytes before expected"));
        self.remaining = &self.remaining[len..];
        value
    }

    fn i16(&mut self) -> i16 {
        i16::from_be_bytes(self.take(2).try_into().expect("two-byte COPY integer"))
    }

    fn i32(&mut self) -> i32 {
        i32::from_be_bytes(self.take(4).try_into().expect("four-byte COPY integer"))
    }
}

fn single_field_from_binary_copy(payload: &[u8]) -> Vec<u8> {
    let mut reader = CopyReader::new(payload);
    assert_eq!(
        reader.take(BINARY_COPY_SIGNATURE.len()),
        BINARY_COPY_SIGNATURE
    );
    assert_eq!(reader.i32(), 0, "binary COPY carried unsupported flags");

    let extension_len = reader.i32();
    assert!(
        extension_len >= 0,
        "binary COPY carried a negative extension length"
    );
    reader.take(usize::try_from(extension_len).expect("nonnegative extension length"));

    assert_eq!(reader.i16(), 1, "binary COPY did not contain one field");
    let field_len = reader.i32();
    assert!(field_len >= 0, "binary COPY field was NULL");
    let field = reader
        .take(usize::try_from(field_len).expect("nonnegative field length"))
        .to_vec();

    assert_eq!(reader.i16(), -1, "binary COPY did not end after one row");
    assert!(
        reader.remaining.is_empty(),
        "binary COPY carried bytes after its trailer"
    );
    field
}

async fn server_wire(client: &compio_postgres::Client, expression: &str) -> Vec<u8> {
    single_field_from_binary_copy(&binary_copy_payload(client, expression).await)
}

fn one_field_binary_copy(payload: &[u8]) -> bytes::Bytes {
    let field_len = i32::try_from(payload.len()).expect("COPY field length fits i32");
    let mut copy = Vec::with_capacity(BINARY_COPY_SIGNATURE.len() + 18 + payload.len());
    copy.extend_from_slice(BINARY_COPY_SIGNATURE);
    copy.extend_from_slice(&0_i32.to_be_bytes());
    copy.extend_from_slice(&0_i32.to_be_bytes());
    copy.extend_from_slice(&1_i16.to_be_bytes());
    copy.extend_from_slice(&field_len.to_be_bytes());
    copy.extend_from_slice(payload);
    copy.extend_from_slice(&(-1_i16).to_be_bytes());
    copy.into()
}

#[derive(Debug)]
struct CopyFeedback {
    equal: bool,
    stored_text: String,
    stored_wire: Vec<u8>,
}

#[allow(clippy::future_not_send)]
async fn copy_feedback(
    client: &compio_postgres::Client,
    label: &str,
    sql_type: &str,
    payload: &[u8],
    expected_expression: &str,
) -> CopyFeedback {
    let table = common::test_object_name(&format!("cpg_wire_feedback_{label}"));
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table}; \
             CREATE TEMPORARY TABLE {table} (value {sql_type})"
        ))
        .await
        .expect("create binary COPY feedback table");

    let mut sink = Box::pin(
        client
            .copy_in(&format!(
                "COPY {table} (value) FROM STDIN WITH (FORMAT binary)"
            ))
            .await
            .expect("start binary COPY feedback"),
    );
    sink.as_mut()
        .send(one_field_binary_copy(payload))
        .await
        .expect("send binary COPY feedback row");
    assert_eq!(
        sink.as_mut()
            .finish()
            .await
            .expect("finish binary COPY feedback"),
        1
    );

    let row = client
        .query_one(
            &format!("SELECT value = {expected_expression}, value::text FROM {table}"),
            &[],
        )
        .await
        .expect("inspect binary COPY feedback value");
    let stored_wire = server_wire(client, &format!("(SELECT value FROM {table})")).await;
    let feedback = CopyFeedback {
        equal: row.get(0),
        stored_text: row.get(1),
        stored_wire,
    };
    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .expect("drop binary COPY feedback table");
    feedback
}

fn outbound_wire<T>(value: &T, ty: &Type) -> Vec<u8>
where
    T: ToSql,
{
    let mut wire = types::private::BytesMut::new();
    let is_null = <T as ToSql>::to_sql(value, ty, &mut wire).expect("encode direct ToSql wire");
    assert!(matches!(is_null, IsNull::No));
    wire.to_vec()
}

#[allow(clippy::future_not_send)]
async fn assert_server_wire<T>(
    client: &compio_postgres::Client,
    name: &str,
    expression: &str,
    value: &T,
    ty: &Type,
) where
    T: ToSql,
{
    assert_eq!(
        outbound_wire(value, ty),
        server_wire(client, expression).await,
        "{name}: direct ToSql bytes differ from binary COPY"
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        })
}

/// The parser's independent hand-check: `int4 1` is four big-endian bytes
/// behind a four-byte length in the documented binary COPY envelope.
#[compio::test]
async fn binary_copy_parser_extracts_known_int4_payload() {
    let client = compio_client().await;
    let payload = binary_copy_payload(&client, "1::int4").await;
    assert_eq!(
        hex(&payload),
        "5047434f50590aff0d0a00000000000000000000010000000400000001ffff"
    );
    assert_eq!(single_field_from_binary_copy(&payload), 1_i32.to_be_bytes());
}

const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xc000;
const NUMERIC_PINF: u16 = 0xd000;
const NUMERIC_NINF: u16 = 0xf000;

#[derive(Clone, Debug)]
struct NumericWireFixture {
    digits: Vec<u16>,
    weight: i16,
    sign: u16,
    display_scale: u16,
}

impl ToSql for NumericWireFixture {
    fn to_sql(&self, _: &Type, out: &mut types::private::BytesMut) -> Result<IsNull, BoxError> {
        let count = i16::try_from(self.digits.len()).map_err(|_| "too many numeric digits")?;
        if self.digits.iter().any(|digit| *digit >= 10_000) {
            return Err("numeric base-10000 digit is out of range".into());
        }
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&self.weight.to_be_bytes());
        out.extend_from_slice(&self.sign.to_be_bytes());
        out.extend_from_slice(&self.display_scale.to_be_bytes());
        for digit in &self.digits {
            out.extend_from_slice(&digit.to_be_bytes());
        }
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut types::private::BytesMut,
    ) -> Result<IsNull, BoxError> {
        <Self as ToSql>::to_sql(self, ty, out)
    }
}

struct NumericCase {
    name: &'static str,
    expression: &'static str,
    value: NumericWireFixture,
}

fn numeric_case(
    name: &'static str,
    expression: &'static str,
    digits: &[u16],
    weight: i16,
    sign: u16,
    display_scale: u16,
) -> NumericCase {
    NumericCase {
        name,
        expression,
        value: NumericWireFixture {
            digits: digits.to_vec(),
            weight,
            sign,
            display_scale,
        },
    }
}

fn numeric_infinity(name: &'static str, expression: &'static str, sign: u16) -> NumericCase {
    numeric_case(name, expression, &[], 0, sign, 32)
}

fn numeric_cases() -> Vec<NumericCase> {
    vec![
        numeric_case(
            "zero-scale-40",
            "'0.0000000000000000000000000000000000000000'::numeric",
            &[],
            0,
            NUMERIC_POS,
            40,
        ),
        numeric_case(
            "negative-zero-normalizes",
            "'-0.00'::numeric",
            &[],
            0,
            NUMERIC_POS,
            2,
        ),
        numeric_case(
            "trailing-zero-scale",
            "'123.450000'::numeric",
            &[123, 4500],
            0,
            NUMERIC_POS,
            6,
        ),
        numeric_case(
            "negative",
            "'-12345.6789'::numeric",
            &[1, 2345, 6789],
            1,
            NUMERIC_NEG,
            4,
        ),
        numeric_case(
            "positive-weight-one",
            "'10000'::numeric",
            &[1],
            1,
            NUMERIC_POS,
            0,
        ),
        numeric_case(
            "negative-weight-one",
            "'0.0001'::numeric",
            &[1],
            -1,
            NUMERIC_POS,
            4,
        ),
        numeric_case(
            "base-10000-alignment",
            "'0.00001'::numeric",
            &[1000],
            -2,
            NUMERIC_POS,
            5,
        ),
        numeric_case(
            "large-positive-weight",
            "'1e1000'::numeric",
            &[1],
            250,
            NUMERIC_POS,
            0,
        ),
        numeric_case(
            "large-negative-weight",
            "'1e-1000'::numeric",
            &[1],
            -250,
            NUMERIC_POS,
            1000,
        ),
        numeric_case(
            "beyond-float-precision",
            "'1234567890123456789012345678901234567890.123456789012345678901234567890'::numeric",
            &[
                1234, 5678, 9012, 3456, 7890, 1234, 5678, 9012, 3456, 7890, 1234, 5678, 9012, 3456,
                7890, 1234, 5678, 9000,
            ],
            9,
            NUMERIC_POS,
            30,
        ),
        numeric_case("nan", "'NaN'::numeric", &[], 0, NUMERIC_NAN, 0),
        numeric_infinity("positive-infinity", "'Infinity'::numeric", NUMERIC_PINF),
        numeric_infinity("negative-infinity", "'-Infinity'::numeric", NUMERIC_NINF),
    ]
}

/// The manual NUMERIC fixture's base-10000 digits, weight, sign, and display
/// scale match `numeric_send`, including all three special signs.
#[compio::test]
async fn manual_numeric_wire_matches_binary_copy() {
    let client = compio_client().await;
    for case in numeric_cases() {
        let ours = outbound_wire(&case.value, &Type::NUMERIC);
        let server = server_wire(&client, case.expression).await;
        assert_eq!(ours, server, "{}: NUMERIC wire mismatch", case.name);

        let equal: bool = client
            .query_one_scalar(
                &format!("SELECT $1::numeric = {}", case.expression),
                &[&case.value],
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: feed NUMERIC wire back to PostgreSQL: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        assert!(equal, "{}: PostgreSQL changed the NUMERIC value", case.name);
    }
}

const POSTGRES_EPOCH_FROM_UNIX_SECS: u64 = 946_684_800;
const TEMPORAL_SESSION_SQL: &str = "SET TIME ZONE 'UTC'; \
    SET DateStyle = 'ISO, YMD'; \
    SET IntervalStyle = 'postgres'";

/// The always-on `SystemTime` carrier and its infinity wrapper use the same
/// epoch and endpoint bytes as `timestamp_send` and `timestamptz_send`.
#[compio::test]
async fn system_time_to_sql_matches_binary_copy() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    let epoch =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS);
    let before_epoch = epoch - std::time::Duration::from_micros(1);
    let after_epoch = epoch + std::time::Duration::from_micros(1);

    assert_server_wire(
        &client,
        "SystemTime timestamp before epoch",
        "'1999-12-31 23:59:59.999999'::timestamp",
        &before_epoch,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "SystemTime timestamp epoch",
        "'2000-01-01 00:00:00'::timestamp",
        &epoch,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "SystemTime timestamptz after epoch",
        "'2000-01-01 00:00:00.000001+00'::timestamptz",
        &after_epoch,
        &Type::TIMESTAMPTZ,
    )
    .await;

    let timestamp_neg: types::Timestamp<std::time::SystemTime> = types::Timestamp::NegInfinity;
    let timestamp_pos: types::Timestamp<std::time::SystemTime> = types::Timestamp::PosInfinity;
    for (name, expression, value) in [
        (
            "SystemTime timestamp negative infinity",
            "'-infinity'::timestamp",
            timestamp_neg,
        ),
        (
            "SystemTime timestamp positive infinity",
            "'infinity'::timestamp",
            timestamp_pos,
        ),
    ] {
        assert_server_wire(&client, name, expression, &value, &Type::TIMESTAMP).await;
    }
}

#[allow(clippy::future_not_send)]
async fn assert_submicrosecond_truncation<T>(
    client: &compio_postgres::Client,
    label: &str,
    sql_type: &str,
    expression: &str,
    truncated_text: &str,
    value: &T,
    ty: &Type,
) where
    T: ToSql,
{
    let ours = outbound_wire(value, ty);
    let server = server_wire(client, expression).await;
    let ours_microseconds = i64::from_be_bytes(ours.as_slice().try_into().expect("eight bytes"));
    let server_microseconds =
        i64::from_be_bytes(server.as_slice().try_into().expect("eight bytes"));
    assert_eq!(
        server_microseconds,
        ours_microseconds + 1,
        "{label}: expected one-microsecond rounding difference"
    );

    let feedback = copy_feedback(client, label, sql_type, &ours, expression).await;
    assert!(
        !feedback.equal,
        "{label}: truncated wire unexpectedly equals rounded input"
    );
    assert_eq!(
        feedback.stored_text, truncated_text,
        "{label}: stored value"
    );
    assert_eq!(
        feedback.stored_wire, ours,
        "{label}: PostgreSQL altered the valid truncated wire"
    );
}

/// `SystemTime` drops sub-microsecond nanoseconds. Binary COPY accepts those
/// bytes unchanged, but the stored value is 1 us below `PostgreSQL`'s rounded
/// interpretation of the same decimal timestamp.
#[compio::test]
async fn system_time_submicroseconds_are_valid_but_truncated() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    let value = std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS)
        + std::time::Duration::from_nanos(123_456_789);
    assert_submicrosecond_truncation(
        &client,
        "system_time_submicro",
        "timestamp",
        "'2000-01-01 00:00:00.123456789'::timestamp",
        "2000-01-01 00:00:00.123456",
        &value,
        &Type::TIMESTAMP,
    )
    .await;
}

/// Desired invariant blocked because the `SystemTime` codec truncates where
/// `PostgreSQL`'s decimal timestamp input rounds.
#[ignore = "SystemTime truncates sub-microseconds while PostgreSQL rounds"]
#[compio::test]
async fn system_time_submicroseconds_must_match_server_rounding() {
    let client = compio_client().await;
    let value = std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS)
        + std::time::Duration::from_nanos(123_456_789);
    assert_server_wire(
        &client,
        "SystemTime submicrosecond",
        "'2000-01-01 00:00:00.123456789'::timestamp",
        &value,
        &Type::TIMESTAMP,
    )
    .await;
}

/// Chrono's finite carriers emit `PostgreSQL`'s epoch, BC, offset, and
/// microsecond-aligned time bytes exactly.
#[cfg(feature = "with-chrono-0_4")]
#[compio::test]
async fn chrono_to_sql_matches_binary_copy() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    let bc_date = chrono::NaiveDate::from_ymd_opt(0, 1, 1).expect("chrono BC date");
    let leap_date = chrono::NaiveDate::from_ymd_opt(2000, 2, 29).expect("chrono leap date");
    let bc_timestamp = bc_date.and_hms_opt(0, 0, 0).expect("chrono BC timestamp");
    let before_epoch = chrono::NaiveDate::from_ymd_opt(1999, 12, 31)
        .expect("chrono epoch date")
        .and_hms_micro_opt(23, 59, 59, 999_999)
        .expect("chrono timestamp microsecond");
    let offset = chrono::DateTime::parse_from_rfc3339("2001-02-03T04:05:06.123456+05:45")
        .expect("chrono offset timestamp");
    let utc = offset.with_timezone(&chrono::Utc);
    let local = utc.with_timezone(&chrono::Local);
    let midnight = chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("chrono midnight");
    let last = chrono::NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999)
        .expect("chrono last microsecond");

    assert_server_wire(
        &client,
        "chrono BC date",
        "'0001-01-01 BC'::date",
        &bc_date,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono leap date",
        "'2000-02-29'::date",
        &leap_date,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono BC timestamp",
        "'0001-01-01 00:00:00 BC'::timestamp",
        &bc_timestamp,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono timestamp before epoch",
        "'1999-12-31 23:59:59.999999'::timestamp",
        &before_epoch,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono FixedOffset timestamptz",
        "'2001-02-03 04:05:06.123456+05:45'::timestamptz",
        &offset,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono UTC timestamptz",
        "'2001-02-03 04:05:06.123456+05:45'::timestamptz",
        &utc,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono Local timestamptz",
        "'2001-02-03 04:05:06.123456+05:45'::timestamptz",
        &local,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono midnight",
        "'00:00'::time",
        &midnight,
        &Type::TIME,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono last microsecond",
        "'23:59:59.999999'::time",
        &last,
        &Type::TIME,
    )
    .await;
}

/// Chrono-backed infinity and 24:00 wrappers emit the server's sentinel bytes.
#[cfg(feature = "with-chrono-0_4")]
#[compio::test]
async fn chrono_temporal_wrappers_match_binary_copy() {
    let client = compio_client().await;
    let date_neg: types::Date<chrono::NaiveDate> = types::Date::NegInfinity;
    let date_pos: types::Date<chrono::NaiveDate> = types::Date::PosInfinity;
    let timestamp_neg: types::Timestamp<chrono::NaiveDateTime> = types::Timestamp::NegInfinity;
    let timestamp_pos: types::Timestamp<chrono::NaiveDateTime> = types::Timestamp::PosInfinity;
    let timestamptz_neg: types::Timestamp<chrono::DateTime<chrono::Utc>> =
        types::Timestamp::NegInfinity;
    let timestamptz_pos: types::Timestamp<chrono::DateTime<chrono::Utc>> =
        types::Timestamp::PosInfinity;
    let end_of_day: types::TimeOfDay<chrono::NaiveTime> = types::TimeOfDay::EndOfDay;

    assert_server_wire(
        &client,
        "chrono date -infinity",
        "'-infinity'::date",
        &date_neg,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono date infinity",
        "'infinity'::date",
        &date_pos,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono timestamp -infinity",
        "'-infinity'::timestamp",
        &timestamp_neg,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono timestamp infinity",
        "'infinity'::timestamp",
        &timestamp_pos,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono timestamptz -infinity",
        "'-infinity'::timestamptz",
        &timestamptz_neg,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono timestamptz infinity",
        "'infinity'::timestamptz",
        &timestamptz_pos,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "chrono 24:00",
        "'24:00'::time",
        &end_of_day,
        &Type::TIME,
    )
    .await;
}

/// Chrono truncates a valid value which COPY accepts unchanged and stores one
/// microsecond below `PostgreSQL`'s rounded decimal input.
#[cfg(feature = "with-chrono-0_4")]
#[compio::test]
async fn chrono_submicroseconds_are_valid_but_truncated() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    let value = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
        .expect("chrono epoch date")
        .and_hms_nano_opt(0, 0, 0, 123_456_789)
        .expect("chrono submicrosecond timestamp");
    assert_submicrosecond_truncation(
        &client,
        "chrono_submicro",
        "timestamp",
        "'2000-01-01 00:00:00.123456789'::timestamp",
        "2000-01-01 00:00:00.123456",
        &value,
        &Type::TIMESTAMP,
    )
    .await;
}

/// Desired invariant blocked because Chrono truncates where `PostgreSQL` rounds.
#[cfg(feature = "with-chrono-0_4")]
#[ignore = "Chrono truncates sub-microseconds while PostgreSQL rounds"]
#[compio::test]
async fn chrono_submicroseconds_must_match_server_rounding() {
    let client = compio_client().await;
    let value = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
        .expect("chrono epoch date")
        .and_hms_nano_opt(0, 0, 0, 123_456_789)
        .expect("chrono submicrosecond timestamp");
    assert_server_wire(
        &client,
        "chrono submicrosecond",
        "'2000-01-01 00:00:00.123456789'::timestamp",
        &value,
        &Type::TIMESTAMP,
    )
    .await;
}

/// Time's finite carriers emit `PostgreSQL`'s epoch, BC, offset, and
/// microsecond-aligned time bytes exactly.
#[cfg(feature = "with-time-0_3")]
#[compio::test]
async fn time_to_sql_matches_binary_copy() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    let bc_date = time::Date::from_calendar_date(0, time::Month::January, 1).expect("time BC date");
    let leap_date =
        time::Date::from_calendar_date(2000, time::Month::February, 29).expect("time leap date");
    let bc_timestamp = bc_date.with_hms(0, 0, 0).expect("time BC timestamp");
    let before_epoch = time::Date::from_calendar_date(1999, time::Month::December, 31)
        .expect("time epoch date")
        .with_hms_micro(23, 59, 59, 999_999)
        .expect("time timestamp microsecond");
    let offset = time::UtcOffset::from_hms(5, 45, 0).expect("time UTC offset");
    let timestamptz = time::Date::from_calendar_date(2001, time::Month::February, 3)
        .expect("time offset date")
        .with_hms_micro(4, 5, 6, 123_456)
        .expect("time offset timestamp")
        .assume_offset(offset);
    let midnight = time::Time::MIDNIGHT;
    let last = time::Time::from_hms_micro(23, 59, 59, 999_999).expect("time last microsecond");

    assert_server_wire(
        &client,
        "time BC date",
        "'0001-01-01 BC'::date",
        &bc_date,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "time leap date",
        "'2000-02-29'::date",
        &leap_date,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "time BC timestamp",
        "'0001-01-01 00:00:00 BC'::timestamp",
        &bc_timestamp,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "time timestamp before epoch",
        "'1999-12-31 23:59:59.999999'::timestamp",
        &before_epoch,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "time OffsetDateTime",
        "'2001-02-03 04:05:06.123456+05:45'::timestamptz",
        &timestamptz,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "time midnight",
        "'00:00'::time",
        &midnight,
        &Type::TIME,
    )
    .await;
    assert_server_wire(
        &client,
        "time last microsecond",
        "'23:59:59.999999'::time",
        &last,
        &Type::TIME,
    )
    .await;
}

/// Time-backed infinity and 24:00 wrappers emit the server's sentinel bytes.
#[cfg(feature = "with-time-0_3")]
#[compio::test]
async fn time_temporal_wrappers_match_binary_copy() {
    let client = compio_client().await;
    let date_neg: types::Date<time::Date> = types::Date::NegInfinity;
    let date_pos: types::Date<time::Date> = types::Date::PosInfinity;
    let timestamp_neg: types::Timestamp<time::PrimitiveDateTime> = types::Timestamp::NegInfinity;
    let timestamp_pos: types::Timestamp<time::PrimitiveDateTime> = types::Timestamp::PosInfinity;
    let timestamptz_neg: types::Timestamp<time::OffsetDateTime> = types::Timestamp::NegInfinity;
    let timestamptz_pos: types::Timestamp<time::OffsetDateTime> = types::Timestamp::PosInfinity;
    let end_of_day: types::TimeOfDay<time::Time> = types::TimeOfDay::EndOfDay;

    assert_server_wire(
        &client,
        "time date -infinity",
        "'-infinity'::date",
        &date_neg,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "time date infinity",
        "'infinity'::date",
        &date_pos,
        &Type::DATE,
    )
    .await;
    assert_server_wire(
        &client,
        "time timestamp -infinity",
        "'-infinity'::timestamp",
        &timestamp_neg,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "time timestamp infinity",
        "'infinity'::timestamp",
        &timestamp_pos,
        &Type::TIMESTAMP,
    )
    .await;
    assert_server_wire(
        &client,
        "time timestamptz -infinity",
        "'-infinity'::timestamptz",
        &timestamptz_neg,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "time timestamptz infinity",
        "'infinity'::timestamptz",
        &timestamptz_pos,
        &Type::TIMESTAMPTZ,
    )
    .await;
    assert_server_wire(
        &client,
        "time 24:00",
        "'24:00'::time",
        &end_of_day,
        &Type::TIME,
    )
    .await;
}

/// Time truncates a valid value which COPY accepts unchanged and stores one
/// microsecond below `PostgreSQL`'s rounded decimal input.
#[cfg(feature = "with-time-0_3")]
#[compio::test]
async fn time_submicroseconds_are_valid_but_truncated() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    let value = time::Date::from_calendar_date(2000, time::Month::January, 1)
        .expect("time epoch date")
        .with_hms_nano(0, 0, 0, 123_456_789)
        .expect("time submicrosecond timestamp");
    assert_submicrosecond_truncation(
        &client,
        "time_submicro",
        "timestamp",
        "'2000-01-01 00:00:00.123456789'::timestamp",
        "2000-01-01 00:00:00.123456",
        &value,
        &Type::TIMESTAMP,
    )
    .await;
}

/// Desired invariant blocked because Time truncates where `PostgreSQL` rounds.
#[cfg(feature = "with-time-0_3")]
#[ignore = "time truncates sub-microseconds while PostgreSQL rounds"]
#[compio::test]
async fn time_submicroseconds_must_match_server_rounding() {
    let client = compio_client().await;
    let value = time::Date::from_calendar_date(2000, time::Month::January, 1)
        .expect("time epoch date")
        .with_hms_nano(0, 0, 0, 123_456_789)
        .expect("time submicrosecond timestamp");
    assert_server_wire(
        &client,
        "time submicrosecond",
        "'2000-01-01 00:00:00.123456789'::timestamp",
        &value,
        &Type::TIMESTAMP,
    )
    .await;
}

#[derive(Clone, Copy, Debug)]
struct IntervalWireFixture {
    microseconds: i64,
    days: i32,
    months: i32,
}

impl ToSql for IntervalWireFixture {
    fn to_sql(&self, _: &Type, out: &mut types::private::BytesMut) -> Result<IsNull, BoxError> {
        out.extend_from_slice(&self.microseconds.to_be_bytes());
        out.extend_from_slice(&self.days.to_be_bytes());
        out.extend_from_slice(&self.months.to_be_bytes());
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut types::private::BytesMut,
    ) -> Result<IsNull, BoxError> {
        <Self as ToSql>::to_sql(self, ty, out)
    }
}

/// The manual INTERVAL fixture matches the server's microseconds/days/months
/// order. `PostgreSQL` 18's infinity endpoints are compared only where the
/// server supports them; `PostgreSQL` 16 still covers every finite case.
#[compio::test]
async fn manual_interval_wire_matches_binary_copy() {
    let client = compio_client().await;
    client
        .batch_execute(TEMPORAL_SESSION_SQL)
        .await
        .expect("set deterministic temporal rendering");
    for (name, expression, value) in [
        (
            "interval zero",
            "'0'::interval",
            IntervalWireFixture {
                microseconds: 0,
                days: 0,
                months: 0,
            },
        ),
        (
            "interval mixed signs",
            "'1 mon -2 days 03:04:05.000006'::interval",
            IntervalWireFixture {
                microseconds: 11_045_000_006,
                days: -2,
                months: 1,
            },
        ),
        (
            "interval opposite signs",
            "'-2 mons 3 days -04:05:06.000007'::interval",
            IntervalWireFixture {
                microseconds: -14_706_000_007,
                days: 3,
                months: -2,
            },
        ),
    ] {
        assert_server_wire(&client, name, expression, &value, &Type::INTERVAL).await;
    }

    let server_version: i32 = client
        .query_one_scalar("SELECT current_setting('server_version_num')::int4", &[])
        .await
        .expect("read server version for interval infinity");
    if server_version >= 170_000 {
        let negative = IntervalWireFixture {
            microseconds: i64::MIN,
            days: i32::MIN,
            months: i32::MIN,
        };
        let positive = IntervalWireFixture {
            microseconds: i64::MAX,
            days: i32::MAX,
            months: i32::MAX,
        };
        assert_server_wire(
            &client,
            "interval negative infinity",
            "'-infinity'::interval",
            &negative,
            &Type::INTERVAL,
        )
        .await;
        assert_server_wire(
            &client,
            "interval positive infinity",
            "'infinity'::interval",
            &positive,
            &Type::INTERVAL,
        )
        .await;
    }
}
