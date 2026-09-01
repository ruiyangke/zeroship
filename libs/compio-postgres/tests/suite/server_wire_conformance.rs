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
use futures_util::TryStreamExt;

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

fn outbound_wire<T>(value: &T, ty: &Type) -> Vec<u8>
where
    T: ToSql,
{
    let mut wire = types::private::BytesMut::new();
    let is_null = <T as ToSql>::to_sql(value, ty, &mut wire).expect("encode direct ToSql wire");
    assert!(matches!(is_null, IsNull::No));
    wire.to_vec()
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
