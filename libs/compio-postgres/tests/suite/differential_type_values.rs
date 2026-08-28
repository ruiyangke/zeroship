//! Differential value-codec tests against `tokio-postgres` 0.7.18.
//!
//! Both drivers talk to the same live PostgreSQL server. Values cross the
//! extended protocol in binary form, come back through `FromSql`, are rebound
//! through `ToSql`, and come back a second time. Only plain Rust data crosses
//! the runtime boundary.
//!
//! The stock type crate has no Rust carrier for NUMERIC, INTERVAL, ranges,
//! multiranges, or records. Its `Vec<T>` carrier also rejects multiple
//! dimensions and cannot retain array lower bounds. Those values use the
//! local `Wire` carrier below, implemented independently for both copies of
//! `postgres-types`. It copies the binary payload while the assertions decode
//! that payload according to PostgreSQL 18.4's `*_send`/`*_recv` C functions
//! and compare the server's text witness. This is what keeps a shared codec
//! mistake from passing merely because both Rust drivers descend from it.
//!
//! This crate's manifest enables none of tokio-postgres's optional chrono,
//! time, UUID, or JSON codecs. Changing dependency features is outside this
//! task's allowed file scope, so UUID/JSON and the full temporal range use the
//! exact wire carrier; the always-on native `SystemTime` codec is compared
//! directly. The deliberate 24:00 and infinity differences remain explicit.

use std::error::Error;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use compio_postgres::types as compio_types;
use futures_util::FutureExt;
use tokio_postgres::types as tokio_types;

#[allow(unused_imports)]
use crate::common;

const POSTGRES_EPOCH_FROM_UNIX_SECS: u64 = 946_684_800;
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Wire(Vec<u8>);

impl<'a> compio_types::FromSql<'a> for Wire {
    fn from_sql(_: &compio_types::Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(Self(raw.to_vec()))
    }

    fn accepts(_: &compio_types::Type) -> bool {
        true
    }
}

impl compio_types::ToSql for Wire {
    fn to_sql(
        &self,
        _: &compio_types::Type,
        out: &mut compio_types::private::BytesMut,
    ) -> Result<compio_types::IsNull, BoxError> {
        out.extend_from_slice(&self.0);
        Ok(compio_types::IsNull::No)
    }

    fn accepts(_: &compio_types::Type) -> bool {
        true
    }

    fn to_sql_checked(
        &self,
        ty: &compio_types::Type,
        out: &mut compio_types::private::BytesMut,
    ) -> Result<compio_types::IsNull, BoxError> {
        <Self as compio_types::ToSql>::to_sql(self, ty, out)
    }
}

impl<'a> tokio_types::FromSql<'a> for Wire {
    fn from_sql(_: &tokio_types::Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(Self(raw.to_vec()))
    }

    fn accepts(_: &tokio_types::Type) -> bool {
        true
    }
}

impl tokio_types::ToSql for Wire {
    fn to_sql(
        &self,
        _: &tokio_types::Type,
        out: &mut tokio_types::private::BytesMut,
    ) -> Result<tokio_types::IsNull, BoxError> {
        out.extend_from_slice(&self.0);
        Ok(tokio_types::IsNull::No)
    }

    fn accepts(_: &tokio_types::Type) -> bool {
        true
    }

    fn to_sql_checked(
        &self,
        ty: &tokio_types::Type,
        out: &mut tokio_types::private::BytesMut,
    ) -> Result<tokio_types::IsNull, BoxError> {
        <Self as tokio_types::ToSql>::to_sql(self, ty, out)
    }
}

fn on_tokio<T, F, Fut>(url: String, run: F) -> T
where
    T: Send + 'static,
    F: FnOnce(tokio_postgres::Client) -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let result = run(client).await;
            let _ = driver.await;
            result
        })
    })
    .join()
    .expect("the tokio thread panicked")
}

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

#[derive(Clone, Debug)]
struct RawCase {
    name: String,
    expression: String,
    parameter_type: String,
}

impl RawCase {
    fn new(name: &str, expression: &str, parameter_type: &str) -> Self {
        Self {
            name: name.to_owned(),
            expression: expression.to_owned(),
            parameter_type: parameter_type.to_owned(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RawObservation {
    name: String,
    decoded: Wire,
    decoded_text: String,
    rebound: Wire,
    rebound_text: String,
}

fn tokio_raw_round_trips(url: String, cases: Vec<RawCase>) -> Vec<RawObservation> {
    on_tokio(url, move |client| async move {
        client
            .batch_execute(
                "SET TIME ZONE 'UTC'; \
                 SET DateStyle = 'ISO, YMD'; \
                 SET IntervalStyle = 'postgres'",
            )
            .await
            .expect("set deterministic rendering on tokio-postgres");

        let mut observations = Vec::with_capacity(cases.len());
        for case in cases {
            let row = client
                .query_one(
                    &format!(
                        "SELECT {0} AS value, ({0})::text AS rendered",
                        case.expression
                    ),
                    &[],
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{}: tokio decode query: {}",
                        case.name,
                        common::error_chain(&error)
                    )
                });
            let decoded: Wire = row.get("value");
            let decoded_text: String = row.get("rendered");

            let row = client
                .query_one(
                    &format!(
                        "SELECT $1::{0} AS value, ($1::{0})::text AS rendered",
                        case.parameter_type
                    ),
                    &[&decoded],
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{}: tokio rebound query: {}",
                        case.name,
                        common::error_chain(&error)
                    )
                });
            observations.push(RawObservation {
                name: case.name,
                decoded,
                decoded_text,
                rebound: row.get("value"),
                rebound_text: row.get("rendered"),
            });
        }
        observations
    })
}

async fn compio_raw_round_trips(cases: &[RawCase]) -> Vec<RawObservation> {
    let client = compio_client().await;
    client
        .batch_execute(
            "SET TIME ZONE 'UTC'; \
             SET DateStyle = 'ISO, YMD'; \
             SET IntervalStyle = 'postgres'",
        )
        .await
        .expect("set deterministic rendering on compio-postgres");

    let mut observations = Vec::with_capacity(cases.len());
    for case in cases {
        let row = client
            .query_one(
                &format!(
                    "SELECT {0} AS value, ({0})::text AS rendered",
                    case.expression
                ),
                &[],
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: compio decode query: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        let decoded: Wire = row.get("value");
        let decoded_text: String = row.get("rendered");

        let row = client
            .query_one(
                &format!(
                    "SELECT $1::{0} AS value, ($1::{0})::text AS rendered",
                    case.parameter_type
                ),
                &[&decoded],
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: compio rebound query: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        observations.push(RawObservation {
            name: case.name.clone(),
            decoded,
            decoded_text,
            rebound: row.get("value"),
            rebound_text: row.get("rendered"),
        });
    }
    observations
}

async fn raw_differential(cases: Vec<RawCase>) -> Vec<RawObservation> {
    let theirs = tokio_raw_round_trips(common::plaintext_url(), cases.clone());
    let ours = compio_raw_round_trips(&cases).await;

    assert_eq!(
        ours.len(),
        theirs.len(),
        "the drivers answered different numbers of value cases"
    );
    for (case, (ours, theirs)) in cases.iter().zip(ours.iter().zip(&theirs)) {
        assert_eq!(
            ours, theirs,
            "{}: the drivers returned different value observations",
            case.name
        );
        assert_eq!(
            ours.decoded, ours.rebound,
            "{}: decode followed by encode changed the binary value",
            case.name
        );
        assert_eq!(
            ours.decoded_text, ours.rebound_text,
            "{}: the server says the rebound value changed",
            case.name
        );
    }
    ours
}

fn tokio_seeded_round_trips(url: String, ty: String, values: Vec<Wire>) -> Vec<Wire> {
    on_tokio(url, move |client| async move {
        let mut returned = Vec::with_capacity(values.len());
        for value in values {
            returned.push(
                client
                    .query_one(&format!("SELECT $1::{ty}"), &[&value])
                    .await
                    .unwrap_or_else(|error| {
                        panic!("tokio seeded {ty}: {}", common::error_chain(&error))
                    })
                    .get(0),
            );
        }
        returned
    })
}

async fn compio_seeded_round_trips(ty: &str, values: &[Wire]) -> Vec<Wire> {
    let client = compio_client().await;
    let mut returned = Vec::with_capacity(values.len());
    for value in values {
        returned.push(
            client
                .query_one(&format!("SELECT $1::{ty}"), &[value])
                .await
                .unwrap_or_else(|error| {
                    panic!("compio seeded {ty}: {}", common::error_chain(&error))
                })
                .get(0),
        );
    }
    returned
}

async fn seeded_differential(ty: &str, values: Vec<Wire>) -> Vec<Wire> {
    let theirs = tokio_seeded_round_trips(common::plaintext_url(), ty.to_owned(), values.clone());
    let ours = compio_seeded_round_trips(ty, &values).await;
    assert_eq!(ours, theirs, "the drivers disagreed on seeded {ty} values");
    assert_eq!(ours, values, "PostgreSQL changed seeded {ty} wire values");
    ours
}

fn observed<'a>(observations: &'a [RawObservation], name: &str) -> &'a RawObservation {
    observations
        .iter()
        .find(|observation| observation.name == name)
        .unwrap_or_else(|| panic!("no observation named {name}"))
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], BoxError> {
        let value = self.remaining.get(..len).ok_or("short binary value")?;
        self.remaining = &self.remaining[len..];
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, BoxError> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, BoxError> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn u16(&mut self) -> Result<u16, BoxError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn i32(&mut self) -> Result<i32, BoxError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn u32(&mut self) -> Result<u32, BoxError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn i64(&mut self) -> Result<i64, BoxError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }

    fn length_prefixed(&mut self) -> Result<Vec<u8>, BoxError> {
        let len = usize::try_from(self.i32()?)?;
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> Result<(), BoxError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(format!("{} trailing binary bytes", self.remaining.len()).into())
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, PartialEq, Eq)]
struct NumericWire {
    digits: Vec<u16>,
    weight: i16,
    sign: u16,
    display_scale: u16,
}

fn decode_numeric(bytes: &[u8]) -> Result<NumericWire, BoxError> {
    let mut reader = Reader::new(bytes);
    let count = usize::from(reader.u16()?);
    let weight = reader.i16()?;
    let sign = reader.u16()?;
    let display_scale = reader.u16()?;
    let mut digits = Vec::with_capacity(count);
    for _ in 0..count {
        let digit = reader.u16()?;
        if digit >= 10_000 {
            return Err(format!("numeric base-10000 digit {digit} is out of range").into());
        }
        digits.push(digit);
    }
    reader.finish()?;
    Ok(NumericWire {
        digits,
        weight,
        sign,
        display_scale,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ArrayWire {
    has_null: bool,
    element_oid: u32,
    dimensions: Vec<(i32, i32)>,
    values: Vec<Option<Vec<u8>>>,
}

fn decode_array(bytes: &[u8]) -> Result<ArrayWire, BoxError> {
    let mut reader = Reader::new(bytes);
    let dimension_count = usize::try_from(reader.i32()?)?;
    let has_null = reader.i32()? != 0;
    let element_oid = reader.u32()?;
    let mut dimensions = Vec::with_capacity(dimension_count);
    let mut item_count = usize::from(dimension_count == 0);
    for index in 0..dimension_count {
        let len = reader.i32()?;
        let lower_bound = reader.i32()?;
        if len < 0 {
            return Err(format!("array dimension {index} has negative length {len}").into());
        }
        if index == 0 {
            item_count = 1;
        }
        item_count = item_count
            .checked_mul(usize::try_from(len)?)
            .ok_or("array item count overflow")?;
        dimensions.push((len, lower_bound));
    }
    if dimension_count == 0 {
        item_count = 0;
    }

    let mut values = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        let len = reader.i32()?;
        if len == -1 {
            values.push(None);
        } else {
            values.push(Some(reader.take(usize::try_from(len)?)?.to_vec()));
        }
    }
    reader.finish()?;
    Ok(ArrayWire {
        has_null,
        element_oid,
        dimensions,
        values,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct RangeWire {
    flags: u8,
    lower: Option<Vec<u8>>,
    upper: Option<Vec<u8>>,
}

fn decode_range(bytes: &[u8]) -> Result<RangeWire, BoxError> {
    let mut reader = Reader::new(bytes);
    let flags = reader.u8()?;
    let lower = if flags & (RANGE_EMPTY | RANGE_LB_INF) == 0 {
        Some(reader.length_prefixed()?)
    } else {
        None
    };
    let upper = if flags & (RANGE_EMPTY | RANGE_UB_INF) == 0 {
        Some(reader.length_prefixed()?)
    } else {
        None
    };
    reader.finish()?;
    Ok(RangeWire {
        flags,
        lower,
        upper,
    })
}

fn decode_multirange(bytes: &[u8]) -> Result<Vec<RangeWire>, BoxError> {
    let mut reader = Reader::new(bytes);
    let count = usize::try_from(reader.i32()?)?;
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        ranges.push(decode_range(&reader.length_prefixed()?)?);
    }
    reader.finish()?;
    Ok(ranges)
}

#[derive(Debug, PartialEq, Eq)]
struct RecordField {
    oid: u32,
    value: Option<Vec<u8>>,
}

fn decode_record(bytes: &[u8]) -> Result<Vec<RecordField>, BoxError> {
    let mut reader = Reader::new(bytes);
    let count = usize::try_from(reader.i32()?)?;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let oid = reader.u32()?;
        let len = reader.i32()?;
        let value = if len == -1 {
            None
        } else {
            Some(reader.take(usize::try_from(len)?)?.to_vec())
        };
        fields.push(RecordField { oid, value });
    }
    reader.finish()?;
    Ok(fields)
}

#[derive(Debug, PartialEq, Eq)]
struct IntervalWire {
    microseconds: i64,
    days: i32,
    months: i32,
}

fn decode_interval(bytes: &[u8]) -> Result<IntervalWire, BoxError> {
    let mut reader = Reader::new(bytes);
    let interval = IntervalWire {
        microseconds: reader.i64()?,
        days: reader.i32()?,
        months: reader.i32()?,
    };
    reader.finish()?;
    Ok(interval)
}

#[derive(Debug, PartialEq, Eq)]
struct NativeObservation {
    int2: Vec<i16>,
    int4: Vec<i32>,
    int8: Vec<i64>,
    float4_bits: Vec<u32>,
    float8_bits: Vec<u64>,
    text: Vec<String>,
    bytea: Vec<Vec<u8>>,
    oid: Vec<u32>,
    name: Vec<String>,
    char_: Vec<i8>,
}

async fn tokio_round_trip<T>(client: &tokio_postgres::Client, ty: &str, value: &T) -> T
where
    T: tokio_types::ToSql + tokio_types::FromSqlOwned + Sync,
{
    client
        .query_one(&format!("SELECT $1::{ty}"), &[value])
        .await
        .unwrap_or_else(|error| panic!("tokio {ty} round trip: {error}"))
        .get(0)
}

async fn compio_round_trip<T>(client: &compio_postgres::Client, ty: &str, value: &T) -> T
where
    T: compio_types::ToSql + compio_types::FromSqlOwned + Sync,
{
    client
        .query_one(&format!("SELECT $1::{ty}"), &[value])
        .await
        .unwrap_or_else(|error| panic!("compio {ty} round trip: {error}"))
        .get(0)
}

fn tokio_native_observation(url: String) -> NativeObservation {
    on_tokio(url, |client| async move {
        let mut int2 = Vec::new();
        for value in [i16::MIN, -1, 0, i16::MAX] {
            int2.push(tokio_round_trip(&client, "int2", &value).await);
        }
        let mut int4 = Vec::new();
        for value in [i32::MIN, -1, 0, i32::MAX] {
            int4.push(tokio_round_trip(&client, "int4", &value).await);
        }
        let mut int8 = Vec::new();
        for value in [i64::MIN, -1, 0, i64::MAX] {
            int8.push(tokio_round_trip(&client, "int8", &value).await);
        }

        let mut float4_bits = Vec::new();
        for value in [
            f32::from_bits(1),
            f32::from_bits(0x8000_0001),
            -0.0,
            0.0,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::from_bits(0x7fc0_0042),
        ] {
            let returned: f32 = tokio_round_trip(&client, "float4", &value).await;
            float4_bits.push(returned.to_bits());
        }
        let mut float8_bits = Vec::new();
        for value in [
            f64::from_bits(1),
            f64::from_bits(0x8000_0000_0000_0001),
            -0.0,
            0.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::from_bits(0x7ff8_0000_0000_0042),
        ] {
            let returned: f64 = tokio_round_trip(&client, "float8", &value).await;
            float8_bits.push(returned.to_bits());
        }

        let text_cases = [
            "",
            "line one\r\nline two\t\\'\"",
            "e\u{301} / \u{1f600} / \u{1f680}",
        ];
        let mut text = Vec::new();
        for value in text_cases {
            text.push(tokio_round_trip(&client, "text", &value.to_owned()).await);
        }

        let byte_cases = [
            Vec::new(),
            vec![0, 0xff, b'\\', b'\n', 0],
            (0_u8..=255).collect(),
        ];
        let mut bytea = Vec::new();
        for value in byte_cases {
            bytea.push(tokio_round_trip(&client, "bytea", &value).await);
        }

        let mut oid = Vec::new();
        for value in [0_u32, 1, u32::MAX] {
            oid.push(tokio_round_trip(&client, "oid", &value).await);
        }

        let name_cases = ["short".to_owned(), "n".repeat(63), "\u{754c}".repeat(21)];
        let mut name = Vec::new();
        for value in name_cases {
            name.push(tokio_round_trip(&client, "name", &value).await);
        }

        let mut char_ = Vec::new();
        for value in [i8::MIN, -1, 0, 1, i8::MAX] {
            char_.push(tokio_round_trip(&client, "\"char\"", &value).await);
        }

        NativeObservation {
            int2,
            int4,
            int8,
            float4_bits,
            float8_bits,
            text,
            bytea,
            oid,
            name,
            char_,
        }
    })
}

async fn compio_native_observation() -> NativeObservation {
    let client = compio_client().await;
    let mut int2 = Vec::new();
    for value in [i16::MIN, -1, 0, i16::MAX] {
        int2.push(compio_round_trip(&client, "int2", &value).await);
    }
    let mut int4 = Vec::new();
    for value in [i32::MIN, -1, 0, i32::MAX] {
        int4.push(compio_round_trip(&client, "int4", &value).await);
    }
    let mut int8 = Vec::new();
    for value in [i64::MIN, -1, 0, i64::MAX] {
        int8.push(compio_round_trip(&client, "int8", &value).await);
    }

    let mut float4_bits = Vec::new();
    for value in [
        f32::from_bits(1),
        f32::from_bits(0x8000_0001),
        -0.0,
        0.0,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::from_bits(0x7fc0_0042),
    ] {
        let returned: f32 = compio_round_trip(&client, "float4", &value).await;
        float4_bits.push(returned.to_bits());
    }
    let mut float8_bits = Vec::new();
    for value in [
        f64::from_bits(1),
        f64::from_bits(0x8000_0000_0000_0001),
        -0.0,
        0.0,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::from_bits(0x7ff8_0000_0000_0042),
    ] {
        let returned: f64 = compio_round_trip(&client, "float8", &value).await;
        float8_bits.push(returned.to_bits());
    }

    let text_cases = [
        "",
        "line one\r\nline two\t\\'\"",
        "e\u{301} / \u{1f600} / \u{1f680}",
    ];
    let mut text = Vec::new();
    for value in text_cases {
        text.push(compio_round_trip(&client, "text", &value.to_owned()).await);
    }

    let byte_cases = [
        Vec::new(),
        vec![0, 0xff, b'\\', b'\n', 0],
        (0_u8..=255).collect(),
    ];
    let mut bytea = Vec::new();
    for value in byte_cases {
        bytea.push(compio_round_trip(&client, "bytea", &value).await);
    }

    let mut oid = Vec::new();
    for value in [0_u32, 1, u32::MAX] {
        oid.push(compio_round_trip(&client, "oid", &value).await);
    }

    let name_cases = ["short".to_owned(), "n".repeat(63), "\u{754c}".repeat(21)];
    let mut name = Vec::new();
    for value in name_cases {
        name.push(compio_round_trip(&client, "name", &value).await);
    }

    let mut char_ = Vec::new();
    for value in [i8::MIN, -1, 0, 1, i8::MAX] {
        char_.push(compio_round_trip(&client, "\"char\"", &value).await);
    }

    NativeObservation {
        int2,
        int4,
        int8,
        float4_bits,
        float8_bits,
        text,
        bytea,
        oid,
        name,
        char_,
    }
}

/// Native scalar codecs must agree at every boundary PostgreSQL preserves.
#[compio::test]
async fn native_scalar_codecs_round_trip_identically() {
    let theirs = tokio_native_observation(common::plaintext_url());
    let ours = compio_native_observation().await;
    assert_eq!(ours, theirs);

    assert_eq!(ours.int2, [i16::MIN, -1, 0, i16::MAX]);
    assert_eq!(ours.int4, [i32::MIN, -1, 0, i32::MAX]);
    assert_eq!(ours.int8, [i64::MIN, -1, 0, i64::MAX]);
    assert_eq!(
        ours.float4_bits,
        [
            1,
            0x8000_0001,
            (-0.0_f32).to_bits(),
            0.0_f32.to_bits(),
            f32::NEG_INFINITY.to_bits(),
            f32::INFINITY.to_bits(),
            0x7fc0_0042,
        ],
        "float4 did not preserve the complete IEEE bit patterns"
    );
    assert_eq!(
        ours.float8_bits,
        [
            1,
            0x8000_0000_0000_0001,
            (-0.0_f64).to_bits(),
            0.0_f64.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            f64::INFINITY.to_bits(),
            0x7ff8_0000_0000_0042,
        ],
        "float8 did not preserve the complete IEEE bit patterns"
    );
    assert_eq!(
        ours.text,
        [
            "".to_owned(),
            "line one\r\nline two\t\\'\"".to_owned(),
            "e\u{301} / \u{1f600} / \u{1f680}".to_owned(),
        ]
    );
    assert_eq!(
        ours.bytea,
        [
            Vec::new(),
            vec![0, 0xff, b'\\', b'\n', 0],
            (0_u8..=255).collect::<Vec<_>>(),
        ]
    );
    assert_eq!(ours.oid, [0, 1, u32::MAX]);
    assert_eq!(
        ours.name,
        ["short".to_owned(), "n".repeat(63), "\u{754c}".repeat(21)]
    );
    assert_eq!(ours.char_, [i8::MIN, -1, 0, 1, i8::MAX]);
}

#[derive(Debug, PartialEq, Eq)]
struct ArrayObservation {
    empty: Vec<Option<String>>,
    nullable: Vec<Option<String>>,
}

fn tokio_array_observation(url: String) -> ArrayObservation {
    on_tokio(url, |client| async move {
        let empty = Vec::<Option<String>>::new();
        let nullable = vec![
            Some("first".to_owned()),
            None,
            Some("line\nlast".to_owned()),
        ];
        ArrayObservation {
            empty: tokio_round_trip(&client, "text[]", &empty).await,
            nullable: tokio_round_trip(&client, "text[]", &nullable).await,
        }
    })
}

async fn compio_array_observation() -> ArrayObservation {
    let client = compio_client().await;
    let empty = Vec::<Option<String>>::new();
    let nullable = vec![
        Some("first".to_owned()),
        None,
        Some("line\nlast".to_owned()),
    ];
    ArrayObservation {
        empty: compio_round_trip(&client, "text[]", &empty).await,
        nullable: compio_round_trip(&client, "text[]", &nullable).await,
    }
}

/// The shared one-dimensional array codec preserves empty and NULL elements.
#[compio::test]
async fn native_one_dimensional_arrays_round_trip_identically() {
    let theirs = tokio_array_observation(common::plaintext_url());
    let ours = compio_array_observation().await;
    assert_eq!(ours, theirs);
    assert!(ours.empty.is_empty());
    assert_eq!(
        ours.nullable,
        [
            Some("first".to_owned()),
            None,
            Some("line\nlast".to_owned())
        ]
    );
}

/// NUMERIC is base-10000 digits plus explicit sign and display scale.
#[compio::test]
async fn numeric_values_round_trip_with_exact_scale_and_special_signs() {
    let high_scale = "12345678901234567890.0000000000000000000001234000";
    let observations = raw_differential(vec![
        RawCase::new("numeric-zero", "0::numeric", "numeric"),
        RawCase::new(
            "numeric-high-scale",
            &format!("'{high_scale}'::numeric"),
            "numeric",
        ),
        RawCase::new(
            "numeric-negative-high-scale",
            &format!("'-{high_scale}'::numeric"),
            "numeric",
        ),
        RawCase::new("numeric-nan", "'NaN'::numeric", "numeric"),
        RawCase::new(
            "numeric-positive-infinity",
            "'Infinity'::numeric",
            "numeric",
        ),
        RawCase::new(
            "numeric-negative-infinity",
            "'-Infinity'::numeric",
            "numeric",
        ),
    ])
    .await;

    let high = observed(&observations, "numeric-high-scale");
    assert_eq!(high.decoded_text, high_scale);
    let numeric = decode_numeric(&high.decoded.0).expect("decode high-scale NUMERIC wire value");
    assert_eq!(
        usize::from(numeric.display_scale),
        high_scale.split_once('.').unwrap().1.len()
    );
    assert!(numeric.digits.iter().all(|digit| *digit < 10_000));
    assert_eq!(
        decode_numeric(
            &observed(&observations, "numeric-negative-high-scale")
                .decoded
                .0
        )
        .unwrap()
        .sign,
        0x4000,
        "negative finite NUMERIC lost its sign"
    );

    for (name, sign) in [
        ("numeric-nan", 0xc000),
        ("numeric-positive-infinity", 0xd000),
        ("numeric-negative-infinity", 0xf000),
    ] {
        let observation = observed(&observations, name);
        let numeric = decode_numeric(&observation.decoded.0).expect("decode special NUMERIC");
        assert_eq!(numeric.sign, sign, "{name} used the wrong sign code");
        assert!(
            numeric.digits.is_empty(),
            "{name} unexpectedly carried digits"
        );
        assert_eq!(numeric.weight, 0, "{name} unexpectedly carried a weight");
    }
}

/// UUID is 16 bytes; JSON preserves input order while JSONB canonicalizes it.
#[compio::test]
async fn uuid_json_and_jsonb_round_trip_with_their_distinct_wire_contracts() {
    let observations = raw_differential(vec![
        RawCase::new(
            "uuid",
            "'00112233-4455-6677-8899-aabbccddeeff'::uuid",
            "uuid",
        ),
        RawCase::new("json", "$${\"aa\":1,\"b\":2,\"a\":3}$$::json", "json"),
        RawCase::new("jsonb", "$${\"aa\":1,\"b\":2,\"a\":3}$$::jsonb", "jsonb"),
    ])
    .await;

    assert_eq!(
        hex(&observed(&observations, "uuid").decoded.0),
        "00112233445566778899aabbccddeeff"
    );
    let json = observed(&observations, "json");
    assert_eq!(json.decoded.0, br#"{"aa":1,"b":2,"a":3}"#);
    assert_eq!(json.decoded_text, r#"{"aa":1,"b":2,"a":3}"#);

    let jsonb = observed(&observations, "jsonb");
    assert_eq!(jsonb.decoded.0.first(), Some(&1), "JSONB version is not 1");
    assert_eq!(
        std::str::from_utf8(&jsonb.decoded.0[1..]).unwrap(),
        r#"{"a": 3, "b": 2, "aa": 1}"#
    );
    assert_eq!(jsonb.decoded_text, r#"{"a": 3, "b": 2, "aa": 1}"#);
}

/// Array send/receive preserves dimensions, NULLs, and non-default bounds.
#[compio::test]
async fn array_shape_and_lower_bounds_round_trip_without_flattening() {
    let observations = raw_differential(vec![
        RawCase::new("array-empty", "ARRAY[]::text[]", "text[]"),
        RawCase::new(
            "array-null-element",
            "ARRAY['first', NULL, E'line\\nlast']::text[]",
            "text[]",
        ),
        RawCase::new(
            "array-multidimensional",
            "ARRAY[['a', NULL], ['c', 'd']]::text[]",
            "text[]",
        ),
        RawCase::new(
            "array-lower-bound-three",
            "'[3:5]={a,b,c}'::text[]",
            "text[]",
        ),
    ])
    .await;

    let empty = decode_array(&observed(&observations, "array-empty").decoded.0)
        .expect("decode empty array");
    assert!(empty.dimensions.is_empty());
    assert!(empty.values.is_empty());

    let nullable = decode_array(&observed(&observations, "array-null-element").decoded.0)
        .expect("decode nullable array");
    assert_eq!(nullable.dimensions, [(3, 1)]);
    assert!(nullable.has_null);
    assert_eq!(nullable.values[1], None);

    let multidimensional =
        decode_array(&observed(&observations, "array-multidimensional").decoded.0)
            .expect("decode multidimensional array");
    assert_eq!(multidimensional.dimensions, [(2, 1), (2, 1)]);
    assert_eq!(multidimensional.values.len(), 4);
    assert_eq!(multidimensional.values[1], None);

    let lower_bound = decode_array(&observed(&observations, "array-lower-bound-three").decoded.0)
        .expect("decode non-1 lower-bound array");
    assert_eq!(lower_bound.dimensions, [(3, 3)]);
    assert_eq!(
        observed(&observations, "array-lower-bound-three").decoded_text,
        "[3:5]={a,b,c}"
    );
}

/// Range flags and multirange member framing survive a binary rebound.
#[compio::test]
async fn ranges_and_multiranges_round_trip_empty_unbounded_and_inclusive_bounds() {
    let observations = raw_differential(vec![
        RawCase::new("range-empty", "'empty'::numrange", "numrange"),
        RawCase::new("range-unbounded", "'(,)'::numrange", "numrange"),
        RawCase::new(
            "range-upper-inclusive",
            "'(1.25,9.75]'::numrange",
            "numrange",
        ),
        RawCase::new(
            "range-lower-inclusive",
            "'[1.25,9.75)'::numrange",
            "numrange",
        ),
        RawCase::new("multirange-empty", "'{}'::nummultirange", "nummultirange"),
        RawCase::new(
            "multirange-bounds",
            "'{(,0],[1.25,2.5),[5,)}'::nummultirange",
            "nummultirange",
        ),
    ])
    .await;

    assert_eq!(
        decode_range(&observed(&observations, "range-empty").decoded.0)
            .unwrap()
            .flags,
        RANGE_EMPTY
    );
    assert_eq!(
        decode_range(&observed(&observations, "range-unbounded").decoded.0)
            .unwrap()
            .flags,
        RANGE_LB_INF | RANGE_UB_INF
    );
    assert_eq!(
        decode_range(&observed(&observations, "range-upper-inclusive").decoded.0)
            .unwrap()
            .flags,
        RANGE_UB_INC
    );
    assert_eq!(
        decode_range(&observed(&observations, "range-lower-inclusive").decoded.0)
            .unwrap()
            .flags,
        RANGE_LB_INC
    );
    assert!(
        decode_multirange(&observed(&observations, "multirange-empty").decoded.0)
            .unwrap()
            .is_empty()
    );
    let ranges =
        decode_multirange(&observed(&observations, "multirange-bounds").decoded.0).unwrap();
    assert_eq!(ranges.len(), 3);
    assert_eq!(ranges[0].flags, RANGE_LB_INF | RANGE_UB_INC);
    assert_eq!(ranges[1].flags, RANGE_LB_INC);
    assert_eq!(ranges[2].flags, RANGE_LB_INC | RANGE_UB_INF);
}

/// A named composite can be decoded and rebound; each field carries its OID.
#[compio::test]
async fn named_composite_values_round_trip_field_for_field() {
    let setup = compio_client().await;
    let composite = common::test_object_name("cpg_diff_composite");
    setup
        .batch_execute(&format!(
            "CREATE TYPE {composite} AS (id int4, label text, amount int8)"
        ))
        .await
        .expect("create shared composite type");

    let observations = raw_differential(vec![RawCase::new(
        "named-composite",
        &format!("ROW(7, E'line\\nvalue', NULL)::{composite}"),
        &composite,
    )])
    .await;
    let fields = decode_record(&observed(&observations, "named-composite").decoded.0)
        .expect("decode composite record");
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].oid, compio_types::Type::INT4.oid());
    assert_eq!(fields[0].value, Some(7_i32.to_be_bytes().to_vec()));
    assert_eq!(fields[1].oid, compio_types::Type::TEXT.oid());
    assert_eq!(fields[1].value, Some(b"line\nvalue".to_vec()));
    assert_eq!(fields[2].oid, compio_types::Type::INT8.oid());
    assert_eq!(fields[2].value, None);

    setup
        .batch_execute(&format!("DROP TYPE {composite} CASCADE"))
        .await
        .expect("drop shared composite type");
}

/// Domains reuse their base type's binary payload across every value family.
#[compio::test]
async fn domains_over_every_value_family_round_trip_as_their_base_wire_type() {
    let setup = compio_client().await;
    let prefix = common::test_object_name("cpg_diff_domain");
    let composite = format!("{prefix}_composite_base");
    setup
        .batch_execute(&format!("CREATE TYPE {composite} AS (id int4, label text)"))
        .await
        .expect("create domain composite base");

    let specifications = vec![
        ("int2", "int2".to_owned(), "(-32768)::int2".to_owned()),
        ("int4", "int4".to_owned(), "(-2147483648)::int4".to_owned()),
        (
            "int8",
            "int8".to_owned(),
            "(-9223372036854775808)::int8".to_owned(),
        ),
        ("numeric", "numeric".to_owned(), "'NaN'::numeric".to_owned()),
        ("float4", "float4".to_owned(), "'-0'::float4".to_owned()),
        ("float8", "float8".to_owned(), "'-0'::float8".to_owned()),
        ("text", "text".to_owned(), "E'line\\ntext'::text".to_owned()),
        (
            "bytea",
            "bytea".to_owned(),
            "decode('00ff', 'hex')".to_owned(),
        ),
        (
            "uuid",
            "uuid".to_owned(),
            "'00112233-4455-6677-8899-aabbccddeeff'::uuid".to_owned(),
        ),
        (
            "json",
            "json".to_owned(),
            "$${\"z\":1,\"a\":2}$$::json".to_owned(),
        ),
        (
            "jsonb",
            "jsonb".to_owned(),
            "$${\"z\":1,\"a\":2}$$::jsonb".to_owned(),
        ),
        (
            "array",
            "text[]".to_owned(),
            "'[3:5]={a,b,c}'::text[]".to_owned(),
        ),
        (
            "range",
            "numrange".to_owned(),
            "'(1.25,9.75]'::numrange".to_owned(),
        ),
        (
            "multirange",
            "nummultirange".to_owned(),
            "'{(,0],[5,)}'::nummultirange".to_owned(),
        ),
        (
            "composite",
            composite.clone(),
            format!("ROW(7, 'seven')::{composite}"),
        ),
        (
            "interval",
            "interval".to_owned(),
            "'1 mon -2 days 03:04:05.000006'::interval".to_owned(),
        ),
        (
            "date",
            "date".to_owned(),
            "'4714-11-24 BC'::date".to_owned(),
        ),
        ("time", "time".to_owned(), "'24:00'::time".to_owned()),
        (
            "timestamp",
            "timestamp".to_owned(),
            "'294276-12-31 23:59:59.999999'::timestamp".to_owned(),
        ),
        (
            "timestamptz",
            "timestamptz".to_owned(),
            "'294276-12-31 23:59:59.999999+00'::timestamptz".to_owned(),
        ),
        ("oid", "oid".to_owned(), "4294967295::oid".to_owned()),
        ("name", "name".to_owned(), "'domain-name'::name".to_owned()),
        (
            "char",
            "\"char\"".to_owned(),
            "(-128)::int4::\"char\"".to_owned(),
        ),
    ];

    let mut setup_sql = String::new();
    let mut cases = Vec::with_capacity(specifications.len());
    let mut domains = Vec::with_capacity(specifications.len());
    for (suffix, base, expression) in &specifications {
        let domain = format!("{prefix}_{suffix}");
        setup_sql.push_str(&format!("CREATE DOMAIN {domain} AS {base};"));
        cases.push(RawCase::new(
            &format!("domain-{suffix}"),
            &format!("({expression})::{domain}"),
            &domain,
        ));
        domains.push(domain);
    }
    setup
        .batch_execute(&setup_sql)
        .await
        .expect("create shared domains");

    let observations = raw_differential(cases).await;
    assert_eq!(observations.len(), specifications.len());

    let mut cleanup = String::new();
    for domain in domains.into_iter().rev() {
        cleanup.push_str(&format!("DROP DOMAIN {domain} CASCADE;"));
    }
    cleanup.push_str(&format!("DROP TYPE {composite} CASCADE;"));
    setup
        .batch_execute(&cleanup)
        .await
        .expect("drop shared domains and composite");
}

/// PostgreSQL's temporal units, epoch, endpoints, and interval field order.
#[compio::test]
async fn temporal_values_round_trip_at_postgres_wire_extremes() {
    let observations = raw_differential(vec![
        RawCase::new("date-min", "'4714-11-24 BC'::date", "date"),
        RawCase::new("date-max", "'5874897-12-31'::date", "date"),
        RawCase::new("date-negative-infinity", "'-infinity'::date", "date"),
        RawCase::new("date-positive-infinity", "'infinity'::date", "date"),
        RawCase::new(
            "timestamp-min",
            "'4714-11-24 00:00:00 BC'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "timestamp-max",
            "'294276-12-31 23:59:59.999999'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "timestamp-negative-infinity",
            "'-infinity'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "timestamp-positive-infinity",
            "'infinity'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "timestamptz-min",
            "'4714-11-24 00:00:00+00 BC'::timestamptz",
            "timestamptz",
        ),
        RawCase::new(
            "timestamptz-max",
            "'294276-12-31 23:59:59.999999+00'::timestamptz",
            "timestamptz",
        ),
        RawCase::new(
            "timestamptz-negative-infinity",
            "'-infinity'::timestamptz",
            "timestamptz",
        ),
        RawCase::new(
            "timestamptz-positive-infinity",
            "'infinity'::timestamptz",
            "timestamptz",
        ),
        RawCase::new("time-midnight", "'00:00'::time", "time"),
        RawCase::new("time-end-of-day", "'24:00'::time", "time"),
        RawCase::new(
            "interval-mixed-signs",
            "'1 mon -2 days 03:04:05.000006'::interval",
            "interval",
        ),
    ])
    .await;

    for (name, expected) in [
        ("date-min", "ffda97a7"),
        ("date-max", "7fda970c"),
        ("date-negative-infinity", "80000000"),
        ("date-positive-infinity", "7fffffff"),
        ("timestamp-min", "fd0f7cc1411fa000"),
        ("timestamp-max", "7fffff5bb3b29fff"),
        ("timestamp-negative-infinity", "8000000000000000"),
        ("timestamp-positive-infinity", "7fffffffffffffff"),
        ("timestamptz-min", "fd0f7cc1411fa000"),
        ("timestamptz-max", "7fffff5bb3b29fff"),
        ("timestamptz-negative-infinity", "8000000000000000"),
        ("timestamptz-positive-infinity", "7fffffffffffffff"),
        ("time-midnight", "0000000000000000"),
        ("time-end-of-day", "000000141dd76000"),
    ] {
        assert_eq!(
            hex(&observed(&observations, name).decoded.0),
            expected,
            "{name}"
        );
    }

    assert_eq!(
        decode_interval(&observed(&observations, "interval-mixed-signs").decoded.0).unwrap(),
        IntervalWire {
            microseconds: 11_045_000_006,
            days: -2,
            months: 1,
        }
    );
    // PostgreSQL 18 recognizes these triples as interval infinities. The live
    // compatibility server predates their text syntax, so seed the C-defined
    // binary values directly and still make both drivers traverse Bind and
    // DataRow in binary mode.
    let interval_extremes = [
        IntervalWire {
            microseconds: i64::MIN,
            days: i32::MIN,
            months: i32::MIN,
        },
        IntervalWire {
            microseconds: i64::MAX,
            days: i32::MAX,
            months: i32::MAX,
        },
    ];
    let seeded: Vec<Wire> = interval_extremes
        .iter()
        .map(|interval| {
            let mut bytes = Vec::with_capacity(16);
            bytes.extend_from_slice(&interval.microseconds.to_be_bytes());
            bytes.extend_from_slice(&interval.days.to_be_bytes());
            bytes.extend_from_slice(&interval.months.to_be_bytes());
            Wire(bytes)
        })
        .collect();
    let returned = seeded_differential("interval", seeded).await;
    for (wire, expected) in returned.iter().zip(interval_extremes) {
        assert_eq!(decode_interval(&wire.0).unwrap(), expected);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SystemTimeObservation {
    timestamp: Vec<SystemTime>,
    timestamptz: Vec<SystemTime>,
}

fn system_time_cases() -> Vec<SystemTime> {
    let postgres_epoch = UNIX_EPOCH + Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS);
    vec![
        UNIX_EPOCH - Duration::from_micros(1),
        UNIX_EPOCH,
        postgres_epoch - Duration::from_micros(1),
        postgres_epoch,
        postgres_epoch + Duration::from_micros(1),
        UNIX_EPOCH + Duration::from_secs(4_102_444_800),
    ]
}

fn tokio_system_time_observation(url: String) -> SystemTimeObservation {
    on_tokio(url, |client| async move {
        let mut timestamp = Vec::new();
        let mut timestamptz = Vec::new();
        for value in system_time_cases() {
            timestamp.push(tokio_round_trip(&client, "timestamp", &value).await);
            timestamptz.push(tokio_round_trip(&client, "timestamptz", &value).await);
        }
        SystemTimeObservation {
            timestamp,
            timestamptz,
        }
    })
}

async fn compio_system_time_observation() -> SystemTimeObservation {
    let client = compio_client().await;
    let mut timestamp = Vec::new();
    let mut timestamptz = Vec::new();
    for value in system_time_cases() {
        timestamp.push(compio_round_trip(&client, "timestamp", &value).await);
        timestamptz.push(compio_round_trip(&client, "timestamptz", &value).await);
    }
    SystemTimeObservation {
        timestamp,
        timestamptz,
    }
}

/// Finite `SystemTime` uses signed microseconds from PostgreSQL's 2000 epoch.
#[compio::test]
async fn finite_system_time_round_trips_identically_for_both_timestamp_types() {
    let theirs = tokio_system_time_observation(common::plaintext_url());
    let ours = compio_system_time_observation().await;
    assert_eq!(ours, theirs);
    assert_eq!(ours.timestamp, system_time_cases());
    assert_eq!(ours.timestamptz, system_time_cases());
}

#[derive(Debug, PartialEq, Eq)]
enum ValueOutcome<T> {
    Value(T),
    LocalFailure,
    ServerFailure(String),
    Panic,
}

fn tokio_positive_infinity_as_system_time(url: String) -> ValueOutcome<SystemTime> {
    on_tokio(url, |client| async move {
        let row = client
            .query_one("SELECT 'infinity'::timestamp", &[])
            .await
            .expect("tokio reads timestamp infinity");
        match std::panic::catch_unwind(AssertUnwindSafe(|| row.try_get::<_, SystemTime>(0))) {
            Ok(Ok(value)) => ValueOutcome::Value(value),
            Ok(Err(error)) if error.code().is_none() => ValueOutcome::LocalFailure,
            Ok(Err(error)) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
            Err(_) => ValueOutcome::Panic,
        }
    })
}

async fn compio_positive_infinity_as_system_time() -> ValueOutcome<SystemTime> {
    let client = compio_client().await;
    let row = client
        .query_one("SELECT 'infinity'::timestamp", &[])
        .await
        .expect("compio reads timestamp infinity");
    match row.try_get::<_, SystemTime>(0) {
        Ok(value) => ValueOutcome::Value(value),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    }
}

/// Bare `SystemTime` cannot represent infinity; upstream aliases it to a date.
#[compio::test]
async fn bare_system_time_refuses_infinity_here_while_tokio_returns_a_finite_value() {
    let theirs = tokio_positive_infinity_as_system_time(common::plaintext_url());
    let ours = compio_positive_infinity_as_system_time().await;
    assert_eq!(ours, ValueOutcome::LocalFailure);

    let ValueOutcome::Value(theirs) = theirs else {
        panic!("tokio-postgres no longer aliases +infinity: {theirs:?}");
    };
    let postgres_epoch = UNIX_EPOCH + Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS);
    assert_eq!(
        theirs.duration_since(postgres_epoch).unwrap().as_micros(),
        i64::MAX as u128,
        "upstream's wrong finite value is not the timestamp infinity payload"
    );
}

fn tokio_overflowing_system_time(url: String, value: SystemTime) -> ValueOutcome<String> {
    on_tokio(url, move |client| async move {
        match AssertUnwindSafe(client.query_one(
            "SELECT encode(timestamp_send($1::timestamp), 'hex')",
            &[&value],
        ))
        .catch_unwind()
        .await
        {
            Ok(Ok(row)) => ValueOutcome::Value(row.get(0)),
            Ok(Err(error)) if error.code().is_none() => ValueOutcome::LocalFailure,
            Ok(Err(error)) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
            Err(_) => ValueOutcome::Panic,
        }
    })
}

async fn compio_overflowing_system_time(value: SystemTime) -> ValueOutcome<String> {
    let client = compio_client().await;
    match client
        .query_one(
            "SELECT encode(timestamp_send($1::timestamp), 'hex')",
            &[&value],
        )
        .await
    {
        Ok(row) => ValueOutcome::Value(row.get(0)),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    }
}

/// A finite `SystemTime` outside i64 microseconds must not become -infinity.
#[compio::test]
async fn system_time_wire_overflow_is_refused_here_while_tokio_aliases_infinity() {
    let postgres_epoch = UNIX_EPOCH + Duration::from_secs(POSTGRES_EPOCH_FROM_UNIX_SECS);
    let value = postgres_epoch + Duration::from_micros(i64::MAX as u64 + 1);
    let theirs = tokio_overflowing_system_time(common::plaintext_url(), value);
    let ours = compio_overflowing_system_time(value).await;

    assert_eq!(ours, ValueOutcome::LocalFailure);
    assert_eq!(
        theirs,
        ValueOutcome::Value("8000000000000000".to_owned()),
        "tokio-postgres no longer narrows the overflowing count to -infinity"
    );
}

#[derive(Debug, PartialEq, Eq)]
struct DomainTypedObservation {
    scalar: ValueOutcome<i32>,
    domain_over_array: ValueOutcome<Vec<i32>>,
    array_of_domain: ValueOutcome<Vec<i32>>,
}

fn tokio_domain_typed_observation(
    url: String,
    scalar: String,
    array: String,
) -> DomainTypedObservation {
    on_tokio(url, move |client| async move {
        async fn scalar_query(client: &tokio_postgres::Client, sql: &str) -> ValueOutcome<i32> {
            match client.query_one(sql, &[&7_i32]).await {
                Ok(row) => ValueOutcome::Value(row.get(0)),
                Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
                Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
            }
        }
        async fn array_query(client: &tokio_postgres::Client, sql: &str) -> ValueOutcome<Vec<i32>> {
            match client.query_one(sql, &[&vec![1_i32, 2, 3]]).await {
                Ok(row) => ValueOutcome::Value(row.get(0)),
                Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
                Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
            }
        }

        DomainTypedObservation {
            scalar: scalar_query(&client, &format!("SELECT $1::{scalar}")).await,
            domain_over_array: array_query(&client, &format!("SELECT $1::{array}")).await,
            array_of_domain: array_query(&client, &format!("SELECT $1::{scalar}[]")).await,
        }
    })
}

async fn compio_domain_typed_observation(scalar: &str, array: &str) -> DomainTypedObservation {
    let client = compio_client().await;

    let scalar_value = match client
        .query_one(&format!("SELECT $1::{scalar}"), &[&7_i32])
        .await
    {
        Ok(row) => ValueOutcome::Value(row.get(0)),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };
    async fn array_value(client: &compio_postgres::Client, sql: &str) -> ValueOutcome<Vec<i32>> {
        match client.query_one(sql, &[&vec![1_i32, 2, 3]]).await {
            Ok(row) => ValueOutcome::Value(row.get(0)),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        }
    }

    DomainTypedObservation {
        scalar: scalar_value,
        domain_over_array: array_value(&client, &format!("SELECT $1::{array}")).await,
        array_of_domain: array_value(&client, &format!("SELECT $1::{scalar}[]")).await,
    }
}

/// PostgreSQL domains use base wire bytes; this driver accepts base carriers.
#[compio::test]
async fn base_codecs_fit_domain_wire_values_but_tokio_rejects_them() {
    let setup = compio_client().await;
    let scalar = common::test_object_name("cpg_diff_typed_domain");
    let array = common::test_object_name("cpg_diff_typed_array_domain");
    setup
        .batch_execute(&format!(
            "CREATE DOMAIN {scalar} AS int4 CHECK (VALUE > 0); \
             CREATE DOMAIN {array} AS int4[]"
        ))
        .await
        .expect("create shared typed domains");

    let theirs =
        tokio_domain_typed_observation(common::plaintext_url(), scalar.clone(), array.clone());
    let ours = compio_domain_typed_observation(&scalar, &array).await;
    assert_eq!(
        ours,
        DomainTypedObservation {
            scalar: ValueOutcome::Value(7),
            domain_over_array: ValueOutcome::Value(vec![1, 2, 3]),
            array_of_domain: ValueOutcome::Value(vec![1, 2, 3]),
        }
    );
    assert_eq!(
        theirs,
        DomainTypedObservation {
            scalar: ValueOutcome::LocalFailure,
            domain_over_array: ValueOutcome::LocalFailure,
            array_of_domain: ValueOutcome::LocalFailure,
        },
        "tokio-postgres unexpectedly learned PostgreSQL's base-domain wire rule"
    );

    setup
        .batch_execute(&format!(
            "DROP DOMAIN {array} CASCADE; DROP DOMAIN {scalar} CASCADE"
        ))
        .await
        .expect("drop shared typed domains");
}

fn tokio_record_array(url: String) -> ValueOutcome<Vec<Vec<RecordField>>> {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(
                "SELECT ARRAY[ROW(7::int4, 'x'::text), \
                              ROW(8::int4, NULL::text)]",
                &[],
            )
            .await
            .expect("tokio reads record array row");
        match row.try_get::<_, Vec<Wire>>(0) {
            Ok(values) => ValueOutcome::Value(
                values
                    .iter()
                    .map(|value| decode_record(&value.0).expect("decode tokio record"))
                    .collect(),
            ),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        }
    })
}

async fn compio_record_array() -> ValueOutcome<Vec<Vec<RecordField>>> {
    let client = compio_client().await;
    let row = client
        .query_one(
            "SELECT ARRAY[ROW(7::int4, 'x'::text), \
                          ROW(8::int4, NULL::text)]",
            &[],
        )
        .await
        .expect("compio reads record array row");
    match row.try_get::<_, Vec<Wire>>(0) {
        Ok(values) => ValueOutcome::Value(
            values
                .iter()
                .map(|value| decode_record(&value.0).expect("decode compio record"))
                .collect(),
        ),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    }
}

/// `_record` is an array type here; upstream incorrectly calls it pseudo.
#[compio::test]
async fn record_arrays_decode_here_while_tokio_misclassifies_them() {
    let theirs = tokio_record_array(common::plaintext_url());
    let ours = compio_record_array().await;
    let ValueOutcome::Value(records) = ours else {
        panic!("this driver did not decode PostgreSQL's record array: {ours:?}");
    };
    assert_eq!(records.len(), 2);
    assert_eq!(records[0][0].value, Some(7_i32.to_be_bytes().to_vec()));
    assert_eq!(records[0][1].value, Some(b"x".to_vec()));
    assert_eq!(records[1][0].value, Some(8_i32.to_be_bytes().to_vec()));
    assert_eq!(records[1][1].value, None);
    assert_eq!(theirs, ValueOutcome::LocalFailure);
}
