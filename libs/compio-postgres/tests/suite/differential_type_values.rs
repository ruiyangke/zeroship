//! Differential value-codec tests against `tokio-postgres` 0.7.18.
//!
//! Both drivers talk to the same live `PostgreSQL` server. Values cross the
//! extended protocol in binary form and the simple protocol in text form;
//! binary and explicitly text-formatted Bind parameters rebound each value.
//! Only plain Rust data crosses the runtime boundary.
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
//! The oracle enables its chrono and time codecs so the optional temporal
//! carriers are genuinely compared when this crate's matching features are
//! selected. Other optional native codecs follow this crate's matching
//! features, while UUID and JSON retain independent exact-wire coverage. The
//! deliberate 24:00 and `SystemTime` infinity differences remain explicit.

use std::error::Error;
use std::future::Future;
use std::net::IpAddr;
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

/// UTF-8 bytes explicitly tagged as a text-format Bind parameter.
///
/// The two drivers use separate copies of `postgres-types`, so this carrier
/// implements both copies of `ToSql`. A built-in value would choose binary;
/// returning `Format::Text` here is what makes the text-encode arm exercise
/// the protocol format code rather than merely cast a binary parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TextWire(String);

impl compio_types::ToSql for TextWire {
    fn to_sql(
        &self,
        _: &compio_types::Type,
        out: &mut compio_types::private::BytesMut,
    ) -> Result<compio_types::IsNull, BoxError> {
        out.extend_from_slice(self.0.as_bytes());
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

    fn encode_format(&self, _: &compio_types::Type) -> compio_types::Format {
        compio_types::Format::Text
    }
}

impl tokio_types::ToSql for TextWire {
    fn to_sql(
        &self,
        _: &tokio_types::Type,
        out: &mut tokio_types::private::BytesMut,
    ) -> Result<tokio_types::IsNull, BoxError> {
        out.extend_from_slice(self.0.as_bytes());
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

    fn encode_format(&self, _: &tokio_types::Type) -> tokio_types::Format {
        tokio_types::Format::Text
    }
}

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

/// One value observed through all four protocol format/direction cells.
///
/// Extended queries request binary results in both drivers. The text decode
/// therefore comes from the simple protocol, while `TextWire` selects a text
/// Bind parameter for the text encode. Every rebound still returns in binary,
/// which lets the server's parser prove that text and binary name one value.
#[derive(Debug, PartialEq, Eq)]
struct FormatObservation {
    name: String,
    binary_decoded: Wire,
    binary_decoded_text: String,
    text_decoded: String,
    binary_encoded: Wire,
    binary_encoded_text: String,
    text_encoded: Wire,
    text_encoded_text: String,
}

fn tokio_simple_value(
    messages: Vec<tokio_postgres::SimpleQueryMessage>,
    case_name: &str,
) -> String {
    let mut values = messages.into_iter().filter_map(|message| match message {
        tokio_postgres::SimpleQueryMessage::Row(row) => Some(
            row.get(0)
                .unwrap_or_else(|| panic!("{case_name}: tokio text result was NULL"))
                .to_owned(),
        ),
        _ => None,
    });
    let value = values
        .next()
        .unwrap_or_else(|| panic!("{case_name}: tokio text decode returned no row"));
    assert!(
        values.next().is_none(),
        "{case_name}: tokio text decode returned more than one row"
    );
    value
}

fn compio_simple_value(
    messages: Vec<compio_postgres::SimpleQueryMessage>,
    case_name: &str,
) -> String {
    let mut values = messages.into_iter().filter_map(|message| match message {
        compio_postgres::SimpleQueryMessage::Row(row) => Some(
            row.get(0)
                .unwrap_or_else(|| panic!("{case_name}: compio text result was NULL"))
                .to_owned(),
        ),
        _ => None,
    });
    let value = values
        .next()
        .unwrap_or_else(|| panic!("{case_name}: compio text decode returned no row"));
    assert!(
        values.next().is_none(),
        "{case_name}: compio text decode returned more than one row"
    );
    value
}

const FORMAT_RENDERING_SQL: &str = "SET TIME ZONE 'UTC'; \
     SET DateStyle = 'ISO, YMD'; \
     SET IntervalStyle = 'postgres'; \
     SET bytea_output = 'hex'; \
     SET lc_monetary = 'C'";

fn tokio_format_observations(url: String, cases: Vec<RawCase>) -> Vec<FormatObservation> {
    on_tokio(url, move |client| async move {
        client
            .batch_execute(FORMAT_RENDERING_SQL)
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
                        "{}: tokio binary decode: {}",
                        case.name,
                        common::error_chain(&error)
                    )
                });
            let binary_decoded: Wire = row.get("value");
            let binary_decoded_text: String = row.get("rendered");

            let messages = client
                .simple_query(&format!("SELECT {}", case.expression))
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{}: tokio text decode: {}",
                        case.name,
                        common::error_chain(&error)
                    )
                });
            let text_decoded = tokio_simple_value(messages, &case.name);

            let row = client
                .query_one(
                    &format!(
                        "SELECT $1::{0} AS value, ($1::{0})::text AS rendered",
                        case.parameter_type
                    ),
                    &[&binary_decoded],
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{}: tokio binary encode: {}",
                        case.name,
                        common::error_chain(&error)
                    )
                });
            let binary_encoded = row.get("value");
            let binary_encoded_text = row.get("rendered");

            let text_parameter = TextWire(text_decoded.clone());
            let row = client
                .query_one(
                    &format!(
                        "SELECT $1::{0} AS value, ($1::{0})::text AS rendered",
                        case.parameter_type
                    ),
                    &[&text_parameter],
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{}: tokio text encode: {}",
                        case.name,
                        common::error_chain(&error)
                    )
                });
            observations.push(FormatObservation {
                name: case.name,
                binary_decoded,
                binary_decoded_text,
                text_decoded,
                binary_encoded,
                binary_encoded_text,
                text_encoded: row.get("value"),
                text_encoded_text: row.get("rendered"),
            });
        }
        observations
    })
}

#[allow(clippy::future_not_send)]
async fn compio_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    let client = compio_client().await;
    client
        .batch_execute(FORMAT_RENDERING_SQL)
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
                    "{}: compio binary decode: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        let binary_decoded: Wire = row.get("value");
        let binary_decoded_text: String = row.get("rendered");

        let messages = client
            .simple_query(&format!("SELECT {}", case.expression))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: compio text decode: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        let text_decoded = compio_simple_value(messages, &case.name);

        let row = client
            .query_one(
                &format!(
                    "SELECT $1::{0} AS value, ($1::{0})::text AS rendered",
                    case.parameter_type
                ),
                &[&binary_decoded],
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: compio binary encode: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        let binary_encoded = row.get("value");
        let binary_encoded_text = row.get("rendered");

        let text_parameter = TextWire(text_decoded.clone());
        let row = client
            .query_one(
                &format!(
                    "SELECT $1::{0} AS value, ($1::{0})::text AS rendered",
                    case.parameter_type
                ),
                &[&text_parameter],
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: compio text encode: {}",
                    case.name,
                    common::error_chain(&error)
                )
            });
        observations.push(FormatObservation {
            name: case.name.clone(),
            binary_decoded,
            binary_decoded_text,
            text_decoded,
            binary_encoded,
            binary_encoded_text,
            text_encoded: row.get("value"),
            text_encoded_text: row.get("rendered"),
        });
    }
    observations
}

fn assert_format_differential(
    cases: &[RawCase],
    ours: &[FormatObservation],
    theirs: &[FormatObservation],
) {
    assert_eq!(
        ours.len(),
        cases.len(),
        "compio-postgres answered the wrong number of format cases"
    );
    assert_eq!(
        theirs.len(),
        cases.len(),
        "tokio-postgres answered the wrong number of format cases"
    );

    for (case, (ours, theirs)) in cases.iter().zip(ours.iter().zip(theirs)) {
        assert_eq!(
            ours, theirs,
            "{}: the drivers disagreed across text/binary encode/decode",
            case.name
        );
        assert_eq!(
            ours.binary_decoded, ours.binary_encoded,
            "{}: binary decode/encode changed the wire value",
            case.name
        );
        assert_eq!(
            ours.binary_decoded, ours.text_encoded,
            "{}: text decode/encode changed the binary value",
            case.name
        );
        assert_eq!(
            ours.text_decoded, ours.binary_decoded_text,
            "{}: text and binary decode rendered different values",
            case.name
        );
        assert_eq!(
            ours.text_decoded, ours.binary_encoded_text,
            "{}: binary rebound rendered a different value",
            case.name
        );
        assert_eq!(
            ours.text_decoded, ours.text_encoded_text,
            "{}: text rebound rendered a different value",
            case.name
        );
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

fn float_format_cases() -> Vec<RawCase> {
    vec![
        RawCase::new("float4-nan", "'NaN'::float4", "float4"),
        RawCase::new("float4-positive-infinity", "'Infinity'::float4", "float4"),
        RawCase::new("float4-negative-infinity", "'-Infinity'::float4", "float4"),
        RawCase::new("float4-negative-zero", "'-0'::float4", "float4"),
        RawCase::new(
            "float4-positive-subnormal",
            "'1.401298464324817e-45'::float4",
            "float4",
        ),
        RawCase::new(
            "float4-negative-subnormal",
            "'-1.401298464324817e-45'::float4",
            "float4",
        ),
        RawCase::new("float8-nan", "'NaN'::float8", "float8"),
        RawCase::new("float8-positive-infinity", "'Infinity'::float8", "float8"),
        RawCase::new("float8-negative-infinity", "'-Infinity'::float8", "float8"),
        RawCase::new("float8-negative-zero", "'-0'::float8", "float8"),
        RawCase::new(
            "float8-positive-subnormal",
            "'4.9406564584124654e-324'::float8",
            "float8",
        ),
        RawCase::new(
            "float8-negative-subnormal",
            "'-4.9406564584124654e-324'::float8",
            "float8",
        ),
        RawCase::new("float8-17-digit", "'1.0000000000000002'::float8", "float8"),
    ]
}

fn tokio_float_format_observations(url: String, cases: Vec<RawCase>) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_float_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

/// Both drivers preserve every required float through text and binary.
#[compio::test]
async fn both_drivers_agree_on_float_text_and_binary_codecs() {
    let cases = float_format_cases();
    let theirs = tokio_float_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_float_format_observations(&cases).await;
    assert_format_differential(&cases, &ours, &theirs);

    let expected = [
        ("float4-nan", "NaN", "7fc00000"),
        ("float4-positive-infinity", "Infinity", "7f800000"),
        ("float4-negative-infinity", "-Infinity", "ff800000"),
        ("float4-negative-zero", "-0", "80000000"),
        ("float4-positive-subnormal", "1e-45", "00000001"),
        ("float4-negative-subnormal", "-1e-45", "80000001"),
        ("float8-nan", "NaN", "7ff8000000000000"),
        ("float8-positive-infinity", "Infinity", "7ff0000000000000"),
        ("float8-negative-infinity", "-Infinity", "fff0000000000000"),
        ("float8-negative-zero", "-0", "8000000000000000"),
        ("float8-positive-subnormal", "5e-324", "0000000000000001"),
        ("float8-negative-subnormal", "-5e-324", "8000000000000001"),
        ("float8-17-digit", "1.0000000000000002", "3ff0000000000001"),
    ];
    assert_eq!(ours.len(), expected.len());
    for (observation, (name, text, binary_hex)) in ours.iter().zip(expected) {
        assert_eq!(observation.name, name);
        assert_eq!(observation.text_decoded, text, "{name}: server text");
        assert_eq!(
            hex(&observation.binary_decoded.0),
            binary_hex,
            "{name}: server binary"
        );
    }
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

fn array_format_cases() -> Vec<RawCase> {
    vec![
        RawCase::new("array-format-empty", "ARRAY[]::text[]", "text[]"),
        RawCase::new(
            "array-format-null-element",
            "ARRAY['first', NULL, E'line\\nlast']::text[]",
            "text[]",
        ),
        RawCase::new(
            "array-format-multidimensional",
            "ARRAY[['a', NULL], ['c', 'd']]::text[]",
            "text[]",
        ),
        RawCase::new(
            "array-format-lower-bound-three",
            "'[3:5]={a,b,c}'::text[]",
            "text[]",
        ),
    ]
}

fn tokio_array_format_observations(url: String, cases: Vec<RawCase>) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_array_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

#[derive(Debug, PartialEq, Eq)]
struct NativeArrayShapeObservation {
    multidimensional: ValueOutcome<Vec<Option<String>>>,
    lower_values: Vec<String>,
    server_lower_bound: i32,
    server_dimensions: String,
    rebound_lower_bound: i32,
    rebound_dimensions: String,
}

fn tokio_native_array_shape_observation(url: String) -> NativeArrayShapeObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(
                "SELECT ARRAY[['a', NULL], ['c', 'd']]::text[], \
                        '[3:5]={a,b,c}'::text[], \
                        array_lower('[3:5]={a,b,c}'::text[], 1), \
                        array_dims('[3:5]={a,b,c}'::text[])",
                &[],
            )
            .await
            .expect("tokio native array decode");
        let multidimensional = match row.try_get::<_, Vec<Option<String>>>(0) {
            Ok(value) => ValueOutcome::Value(value),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };
        let lower_values: Vec<String> = row.get(1);
        let rebound = client
            .query_one(
                "SELECT array_lower($1::text[], 1), array_dims($1::text[])",
                &[&lower_values],
            )
            .await
            .expect("tokio native lower-bound rebound");
        NativeArrayShapeObservation {
            multidimensional,
            lower_values,
            server_lower_bound: row.get(2),
            server_dimensions: row.get(3),
            rebound_lower_bound: rebound.get(0),
            rebound_dimensions: rebound.get(1),
        }
    })
}

#[allow(clippy::future_not_send)]
async fn compio_native_array_shape_observation() -> NativeArrayShapeObservation {
    let client = compio_client().await;
    let row = client
        .query_one(
            "SELECT ARRAY[['a', NULL], ['c', 'd']]::text[], \
                    '[3:5]={a,b,c}'::text[], \
                    array_lower('[3:5]={a,b,c}'::text[], 1), \
                    array_dims('[3:5]={a,b,c}'::text[])",
            &[],
        )
        .await
        .expect("compio native array decode");
    let multidimensional = match row.try_get::<_, Vec<Option<String>>>(0) {
        Ok(value) => ValueOutcome::Value(value),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };
    let lower_values: Vec<String> = row.get(1);
    let rebound = client
        .query_one(
            "SELECT array_lower($1::text[], 1), array_dims($1::text[])",
            &[&lower_values],
        )
        .await
        .expect("compio native lower-bound rebound");
    NativeArrayShapeObservation {
        multidimensional,
        lower_values,
        server_lower_bound: row.get(2),
        server_dimensions: row.get(3),
        rebound_lower_bound: rebound.get(0),
        rebound_dimensions: rebound.get(1),
    }
}

/// Both drivers preserve array wire shape in text and binary formats.
#[compio::test]
async fn both_drivers_agree_on_array_text_and_binary_codecs() {
    let cases = array_format_cases();
    let theirs = tokio_array_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_array_format_observations(&cases).await;
    assert_format_differential(&cases, &ours, &theirs);

    let empty = decode_array(&ours[0].binary_decoded.0).expect("decode empty array");
    assert!(empty.dimensions.is_empty());
    assert!(empty.values.is_empty());

    let nullable = decode_array(&ours[1].binary_decoded.0).expect("decode nullable array");
    assert_eq!(nullable.dimensions, [(3, 1)]);
    assert!(nullable.has_null);
    assert_eq!(nullable.values[1], None);

    let multidimensional =
        decode_array(&ours[2].binary_decoded.0).expect("decode multidimensional array");
    assert_eq!(multidimensional.dimensions, [(2, 1), (2, 1)]);
    assert_eq!(multidimensional.values.len(), 4);
    assert_eq!(ours[2].text_decoded, "{{a,NULL},{c,d}}");

    let lower_bound = decode_array(&ours[3].binary_decoded.0).expect("decode lower-bound array");
    assert_eq!(lower_bound.dimensions, [(3, 3)]);
    assert_eq!(ours[3].text_decoded, "[3:5]={a,b,c}");

    // The wire carrier above proves both drivers preserve the server's shape.
    // Their shared Vec codec cannot represent it: it refuses two dimensions
    // but silently discards a non-1 lower bound and writes 1 on rebound.
    let theirs = tokio_native_array_shape_observation(common::plaintext_url());
    let ours = compio_native_array_shape_observation().await;
    assert_eq!(ours, theirs);
    assert_eq!(ours.multidimensional, ValueOutcome::LocalFailure);
    assert_eq!(ours.lower_values, ["a", "b", "c"]);
    assert_eq!(ours.server_lower_bound, 3);
    assert_eq!(ours.server_dimensions, "[3:5]");
    assert_eq!(ours.rebound_lower_bound, 1);
    assert_eq!(ours.rebound_dimensions, "[1:3]");
}

/// Desired invariant blocked by both Vec codecs discarding array lower bounds.
#[ignore = "both postgres-types Vec codecs normalize [3:5] to [1:3]"]
#[compio::test]
async fn native_array_codecs_must_not_discard_lower_bounds() {
    let theirs = tokio_native_array_shape_observation(common::plaintext_url());
    let ours = compio_native_array_shape_observation().await;
    assert_eq!(ours.server_dimensions, ours.rebound_dimensions);
    assert_eq!(theirs.server_dimensions, theirs.rebound_dimensions);
}

#[cfg(feature = "array-impls")]
#[derive(Debug, PartialEq, Eq)]
struct NativeFixedArrayObservation {
    decoded_exact: [i32; 3],
    decoded_nullable: [Option<i32>; 3],
    decoded_empty: [i32; 0],
    too_few: ValueOutcome<[i32; 3]>,
    too_many: ValueOutcome<[i32; 3]>,
    too_few_error: String,
    too_many_error: String,
    decoded_lower: [i32; 3],
    server_text: [String; 4],
    server_wires: [Wire; 4],
    outbound_wires: [Vec<u8>; 4],
    rebound_exact: [i32; 3],
    rebound_nullable: [Option<i32>; 3],
    rebound_empty: [i32; 0],
    rebound_lower: [i32; 3],
    rebound_text: [String; 4],
    rebound_wires: [Wire; 4],
    server_lower_bound: i32,
    server_dimensions: String,
    rebound_lower_bound: i32,
    rebound_dimensions: String,
    bytea_accepts: bool,
    bytea_array_accepts: bool,
    bytea_server_text: String,
    bytea_server_wire: Wire,
    bytea_outbound_wire: Vec<u8>,
    bytea_rebound_text: String,
    bytea_rebound_wire: Wire,
}

#[cfg(feature = "array-impls")]
const NATIVE_FIXED_ARRAY_DECODE_SQL: &str = "SELECT \
    ARRAY[1,2,3]::int4[], (ARRAY[1,2,3]::int4[])::text, \
    ARRAY[-2147483648,NULL,2147483647]::int4[], \
        (ARRAY[-2147483648,NULL,2147483647]::int4[])::text, \
    ARRAY[]::int4[], (ARRAY[]::int4[])::text, \
    ARRAY[1,2]::int4[], ARRAY[1,2,3,4]::int4[], \
    '[3:5]={1,2,3}'::int4[], ('[3:5]={1,2,3}'::int4[])::text, \
        array_lower('[3:5]={1,2,3}'::int4[], 1), \
        array_dims('[3:5]={1,2,3}'::int4[]), \
    decode('0080ff', 'hex'), (decode('0080ff', 'hex'))::text";

#[cfg(feature = "array-impls")]
const NATIVE_FIXED_ARRAY_REBOUND_SQL: &str = "SELECT \
    $1::int4[], ($1::int4[])::text, \
    $2::int4[], ($2::int4[])::text, \
    $3::int4[], ($3::int4[])::text, \
    $4::int4[], ($4::int4[])::text, \
        array_lower($4::int4[], 1), array_dims($4::int4[]), \
    $5::bytea, ($5::bytea)::text";

#[cfg(feature = "array-impls")]
fn tokio_fixed_array_wire<T>(value: &T, ty: &tokio_types::Type) -> Vec<u8>
where
    T: tokio_types::ToSql,
{
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("tokio-postgres fixed-array encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "array-impls")]
fn compio_fixed_array_wire<T>(value: &T, ty: &compio_types::Type) -> Vec<u8>
where
    T: compio_types::ToSql,
{
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("compio-postgres fixed-array encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "array-impls")]
fn tokio_fixed_array_decode_error(wire: &Wire) -> String {
    <[i32; 3] as tokio_types::FromSql>::from_sql(&tokio_types::Type::INT4_ARRAY, &wire.0)
        .expect_err("tokio-postgres accepted the wrong fixed-array length")
        .to_string()
}

#[cfg(feature = "array-impls")]
fn compio_fixed_array_decode_error(wire: &Wire) -> String {
    <[i32; 3] as compio_types::FromSql>::from_sql(&compio_types::Type::INT4_ARRAY, &wire.0)
        .expect_err("compio-postgres accepted the wrong fixed-array length")
        .to_string()
}

#[cfg(feature = "array-impls")]
fn tokio_native_fixed_array_observation(url: String) -> NativeFixedArrayObservation {
    on_tokio(url, |client| async move {
        client
            .batch_execute(FORMAT_RENDERING_SQL)
            .await
            .expect("set deterministic rendering on tokio-postgres");
        let row = client
            .query_one(NATIVE_FIXED_ARRAY_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres fixed-array decode");
        let decoded_exact: [i32; 3] = row.get(0);
        let decoded_nullable: [Option<i32>; 3] = row.get(2);
        let decoded_empty: [i32; 0] = row.get(4);
        let too_few = match row.try_get::<_, [i32; 3]>(6) {
            Ok(value) => ValueOutcome::Value(value),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };
        let too_many = match row.try_get::<_, [i32; 3]>(7) {
            Ok(value) => ValueOutcome::Value(value),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };
        let too_few_wire: Wire = row.get(6);
        let too_many_wire: Wire = row.get(7);
        let decoded_lower: [i32; 3] = row.get(8);
        let bytea = [0x00_u8, 0x80, 0xff];
        let outbound_wires = [
            tokio_fixed_array_wire(&decoded_exact, &tokio_types::Type::INT4_ARRAY),
            tokio_fixed_array_wire(&decoded_nullable, &tokio_types::Type::INT4_ARRAY),
            tokio_fixed_array_wire(&decoded_empty, &tokio_types::Type::INT4_ARRAY),
            tokio_fixed_array_wire(&decoded_lower, &tokio_types::Type::INT4_ARRAY),
        ];
        let rebound = client
            .query_one(
                NATIVE_FIXED_ARRAY_REBOUND_SQL,
                &[
                    &decoded_exact,
                    &decoded_nullable,
                    &decoded_empty,
                    &decoded_lower,
                    &bytea,
                ],
            )
            .await
            .expect("tokio-postgres fixed-array rebound");
        NativeFixedArrayObservation {
            decoded_exact,
            decoded_nullable,
            decoded_empty,
            too_few,
            too_many,
            too_few_error: tokio_fixed_array_decode_error(&too_few_wire),
            too_many_error: tokio_fixed_array_decode_error(&too_many_wire),
            decoded_lower,
            server_text: [row.get(1), row.get(3), row.get(5), row.get(9)],
            server_wires: [row.get(0), row.get(2), row.get(4), row.get(8)],
            outbound_wires,
            rebound_exact: rebound.get(0),
            rebound_nullable: rebound.get(2),
            rebound_empty: rebound.get(4),
            rebound_lower: rebound.get(6),
            rebound_text: [
                rebound.get(1),
                rebound.get(3),
                rebound.get(5),
                rebound.get(7),
            ],
            rebound_wires: [
                rebound.get(0),
                rebound.get(2),
                rebound.get(4),
                rebound.get(6),
            ],
            server_lower_bound: row.get(10),
            server_dimensions: row.get(11),
            rebound_lower_bound: rebound.get(8),
            rebound_dimensions: rebound.get(9),
            bytea_accepts: <[u8; 3] as tokio_types::ToSql>::accepts(&tokio_types::Type::BYTEA),
            bytea_array_accepts: <[u8; 3] as tokio_types::ToSql>::accepts(
                &tokio_types::Type::BYTEA_ARRAY,
            ),
            bytea_server_text: row.get(13),
            bytea_server_wire: row.get(12),
            bytea_outbound_wire: tokio_fixed_array_wire(&bytea, &tokio_types::Type::BYTEA),
            bytea_rebound_text: rebound.get(11),
            bytea_rebound_wire: rebound.get(10),
        }
    })
}

#[cfg(feature = "array-impls")]
#[allow(clippy::future_not_send)]
async fn compio_native_fixed_array_observation() -> NativeFixedArrayObservation {
    let client = compio_client().await;
    client
        .batch_execute(FORMAT_RENDERING_SQL)
        .await
        .expect("set deterministic rendering on compio-postgres");
    let row = client
        .query_one(NATIVE_FIXED_ARRAY_DECODE_SQL, &[])
        .await
        .expect("compio-postgres fixed-array decode");
    let decoded_exact: [i32; 3] = row.get(0);
    let decoded_nullable: [Option<i32>; 3] = row.get(2);
    let decoded_empty: [i32; 0] = row.get(4);
    let too_few = match row.try_get::<_, [i32; 3]>(6) {
        Ok(value) => ValueOutcome::Value(value),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };
    let too_many = match row.try_get::<_, [i32; 3]>(7) {
        Ok(value) => ValueOutcome::Value(value),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };
    let too_few_wire: Wire = row.get(6);
    let too_many_wire: Wire = row.get(7);
    let decoded_lower: [i32; 3] = row.get(8);
    let bytea = [0x00_u8, 0x80, 0xff];
    let outbound_wires = [
        compio_fixed_array_wire(&decoded_exact, &compio_types::Type::INT4_ARRAY),
        compio_fixed_array_wire(&decoded_nullable, &compio_types::Type::INT4_ARRAY),
        compio_fixed_array_wire(&decoded_empty, &compio_types::Type::INT4_ARRAY),
        compio_fixed_array_wire(&decoded_lower, &compio_types::Type::INT4_ARRAY),
    ];
    let rebound = client
        .query_one(
            NATIVE_FIXED_ARRAY_REBOUND_SQL,
            &[
                &decoded_exact,
                &decoded_nullable,
                &decoded_empty,
                &decoded_lower,
                &bytea,
            ],
        )
        .await
        .expect("compio-postgres fixed-array rebound");
    NativeFixedArrayObservation {
        decoded_exact,
        decoded_nullable,
        decoded_empty,
        too_few,
        too_many,
        too_few_error: compio_fixed_array_decode_error(&too_few_wire),
        too_many_error: compio_fixed_array_decode_error(&too_many_wire),
        decoded_lower,
        server_text: [row.get(1), row.get(3), row.get(5), row.get(9)],
        server_wires: [row.get(0), row.get(2), row.get(4), row.get(8)],
        outbound_wires,
        rebound_exact: rebound.get(0),
        rebound_nullable: rebound.get(2),
        rebound_empty: rebound.get(4),
        rebound_lower: rebound.get(6),
        rebound_text: [
            rebound.get(1),
            rebound.get(3),
            rebound.get(5),
            rebound.get(7),
        ],
        rebound_wires: [
            rebound.get(0),
            rebound.get(2),
            rebound.get(4),
            rebound.get(6),
        ],
        server_lower_bound: row.get(10),
        server_dimensions: row.get(11),
        rebound_lower_bound: rebound.get(8),
        rebound_dimensions: rebound.get(9),
        bytea_accepts: <[u8; 3] as compio_types::ToSql>::accepts(&compio_types::Type::BYTEA),
        bytea_array_accepts: <[u8; 3] as compio_types::ToSql>::accepts(
            &compio_types::Type::BYTEA_ARRAY,
        ),
        bytea_server_text: row.get(13),
        bytea_server_wire: row.get(12),
        bytea_outbound_wire: compio_fixed_array_wire(&bytea, &compio_types::Type::BYTEA),
        bytea_rebound_text: rebound.get(11),
        bytea_rebound_wire: rebound.get(10),
    }
}

/// Fixed arrays agree on values and exact wire bytes, including two shared
/// shape defects that the active assertions pin independently of the oracle.
#[cfg(feature = "array-impls")]
#[compio::test]
async fn native_fixed_array_codecs_match_values_and_pin_shared_wire_defects() {
    let theirs = tokio_native_fixed_array_observation(common::plaintext_url());
    let ours = compio_native_fixed_array_observation().await;
    assert_eq!(ours, theirs);

    assert_eq!(ours.decoded_exact, [1, 2, 3]);
    assert_eq!(
        ours.decoded_nullable,
        [Some(i32::MIN), None, Some(i32::MAX)]
    );
    assert_eq!(ours.decoded_empty, [0_i32; 0]);
    assert_eq!(ours.too_few, ValueOutcome::LocalFailure);
    assert_eq!(ours.too_many, ValueOutcome::LocalFailure);
    assert_eq!(
        ours.too_few_error,
        "too few elements in array (expected 3, got 2)"
    );
    assert_eq!(
        ours.too_many_error,
        "excess elements in array (expected 3, got more than that)"
    );
    assert_eq!(ours.decoded_lower, [1, 2, 3]);
    assert_eq!(
        ours.server_text,
        [
            "{1,2,3}",
            "{-2147483648,NULL,2147483647}",
            "{}",
            "[3:5]={1,2,3}",
        ]
    );

    let exact_wire = "0000000100000000000000170000000300000001\
        000000040000000100000004000000020000000400000003";
    let nullable_wire = "0000000100000001000000170000000300000001\
        0000000480000000ffffffff000000047fffffff";
    let canonical_empty_wire = "000000000000000000000017";
    let noncanonical_empty_wire = "0000000100000000000000170000000000000001";
    let lower_three_wire = "0000000100000000000000170000000300000003\
        000000040000000100000004000000020000000400000003";
    assert_eq!(hex(&ours.server_wires[0].0), exact_wire);
    assert_eq!(hex(&ours.server_wires[1].0), nullable_wire);
    assert_eq!(hex(&ours.server_wires[2].0), canonical_empty_wire);
    assert_eq!(hex(&ours.server_wires[3].0), lower_three_wire);
    assert_eq!(hex(&ours.outbound_wires[0]), exact_wire);
    assert_eq!(hex(&ours.outbound_wires[1]), nullable_wire);

    // Both fixed-array codecs write a one-dimensional, zero-length array that
    // PostgreSQL never emits. The server canonicalizes it back to ndim = 0.
    assert_eq!(hex(&ours.outbound_wires[2]), noncanonical_empty_wire);
    assert_ne!(ours.outbound_wires[2], ours.server_wires[2].0);
    assert_eq!(hex(&ours.rebound_wires[2].0), canonical_empty_wire);

    // A fixed Rust array has no lower-bound slot, so both codecs normalize the
    // server's [3:5] array to [1:3] when writing it back.
    assert_eq!(hex(&ours.outbound_wires[3]), exact_wire);
    assert_ne!(ours.outbound_wires[3], ours.server_wires[3].0);
    assert_eq!(ours.server_lower_bound, 3);
    assert_eq!(ours.server_dimensions, "[3:5]");
    assert_eq!(ours.rebound_lower_bound, 1);
    assert_eq!(ours.rebound_dimensions, "[1:3]");

    assert_eq!(ours.rebound_exact, [1, 2, 3]);
    assert_eq!(
        ours.rebound_nullable,
        [Some(i32::MIN), None, Some(i32::MAX)]
    );
    assert_eq!(ours.rebound_empty, [0_i32; 0]);
    assert_eq!(ours.rebound_lower, [1, 2, 3]);
    assert_eq!(
        ours.rebound_text,
        ["{1,2,3}", "{-2147483648,NULL,2147483647}", "{}", "{1,2,3}",]
    );
    assert_eq!(hex(&ours.rebound_wires[0].0), exact_wire);
    assert_eq!(hex(&ours.rebound_wires[1].0), nullable_wire);
    assert_eq!(hex(&ours.rebound_wires[3].0), exact_wire);

    // `[u8; N]` is deliberately a BYTEA specialization, not a BYTEA[] array.
    assert!(ours.bytea_accepts);
    assert!(!ours.bytea_array_accepts);
    assert_eq!(ours.bytea_server_text, "\\x0080ff");
    assert_eq!(ours.bytea_server_wire.0, [0x00, 0x80, 0xff]);
    assert_eq!(ours.bytea_outbound_wire, [0x00, 0x80, 0xff]);
    assert_eq!(ours.bytea_rebound_text, "\\x0080ff");
    assert_eq!(ours.bytea_rebound_wire.0, [0x00, 0x80, 0xff]);
}

/// Desired invariant blocked by both fixed-array codecs emitting an empty
/// array as one zero-length dimension instead of `PostgreSQL`'s canonical wire.
#[cfg(feature = "array-impls")]
#[ignore = "both postgres-types fixed-array codecs emit noncanonical empty-array wire"]
#[compio::test]
async fn native_fixed_array_codecs_must_emit_canonical_empty_wire() {
    let theirs = tokio_native_fixed_array_observation(common::plaintext_url());
    let ours = compio_native_fixed_array_observation().await;
    assert_eq!(ours.outbound_wires[2], ours.server_wires[2].0);
    assert_eq!(theirs.outbound_wires[2], theirs.server_wires[2].0);
}

/// Desired invariant blocked by both fixed-array codecs discarding array
/// lower bounds that their Rust carrier has no field in which to retain.
#[cfg(feature = "array-impls")]
#[ignore = "both postgres-types fixed-array codecs normalize [3:5] to [1:3]"]
#[compio::test]
async fn native_fixed_array_codecs_must_preserve_lower_bounds() {
    let theirs = tokio_native_fixed_array_observation(common::plaintext_url());
    let ours = compio_native_fixed_array_observation().await;
    assert_eq!(ours.server_dimensions, ours.rebound_dimensions);
    assert_eq!(theirs.server_dimensions, theirs.rebound_dimensions);
}

fn byte_string_format_cases() -> Vec<RawCase> {
    vec![
        RawCase::new("bytea-format-empty", "decode('', 'hex')", "bytea"),
        RawCase::new(
            "bytea-format-high-bytes",
            "decode('00ff5c0a80c3', 'hex')",
            "bytea",
        ),
        RawCase::new("text-format-empty", "''::text", "text"),
        RawCase::new(
            "text-format-hostile-utf8",
            "$cpg$line one\r\nline two\t\\'\" / e\u{301} / \u{1f600} / \u{1f680} / \u{2028}$cpg$::text",
            "text",
        ),
        RawCase::new(
            "varchar-format-trailing-spaces",
            "$cpg$varying  \u{754c}\u{1f680}  $cpg$::varchar(32)",
            "varchar(32)",
        ),
        RawCase::new("char-format-ascii-padding", "'xy'::char(8)", "char(8)"),
        RawCase::new(
            "char-format-unicode-padding",
            "'\u{754c}\u{1f680}'::char(4)",
            "char(4)",
        ),
    ]
}

fn tokio_byte_string_format_observations(
    url: String,
    cases: Vec<RawCase>,
) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_byte_string_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

#[derive(Debug, PartialEq, Eq)]
struct NativeByteStringObservation {
    decoded_bytea: [Vec<u8>; 2],
    rebound_bytea: [Vec<u8>; 2],
    decoded_strings: [String; 5],
    rebound_strings: [String; 5],
}

const NATIVE_BYTE_STRING_DECODE_SQL: &str = "SELECT \
    decode('', 'hex'), \
    decode('00ff5c0a80c3', 'hex'), \
    ''::text, \
    $cpg$line one\r\nline two\t\\'\" / e\u{301} / \u{1f600} / \u{1f680} / \u{2028}$cpg$::text, \
    $cpg$varying  \u{754c}\u{1f680}  $cpg$::varchar(32), \
    'xy'::char(8), \
    '\u{754c}\u{1f680}'::char(4)";

const NATIVE_BYTE_STRING_REBOUND_SQL: &str = "SELECT \
    $1::bytea, $2::bytea, $3::text, $4::text, $5::varchar(32), \
    $6::char(8), $7::char(4)";

fn tokio_native_byte_string_observation(url: String) -> NativeByteStringObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_BYTE_STRING_DECODE_SQL, &[])
            .await
            .expect("tokio native byte/string decode");
        let decoded_bytea = [row.get(0), row.get(1)];
        let decoded_strings = [row.get(2), row.get(3), row.get(4), row.get(5), row.get(6)];
        let ascii_char_input = "xy".to_owned();
        let unicode_char_input = "\u{754c}\u{1f680}".to_owned();
        let rebound = client
            .query_one(
                NATIVE_BYTE_STRING_REBOUND_SQL,
                &[
                    &decoded_bytea[0],
                    &decoded_bytea[1],
                    &decoded_strings[0],
                    &decoded_strings[1],
                    &decoded_strings[2],
                    &ascii_char_input,
                    &unicode_char_input,
                ],
            )
            .await
            .expect("tokio native byte/string encode");
        NativeByteStringObservation {
            decoded_bytea,
            rebound_bytea: [rebound.get(0), rebound.get(1)],
            decoded_strings,
            rebound_strings: [
                rebound.get(2),
                rebound.get(3),
                rebound.get(4),
                rebound.get(5),
                rebound.get(6),
            ],
        }
    })
}

#[allow(clippy::future_not_send)]
async fn compio_native_byte_string_observation() -> NativeByteStringObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_BYTE_STRING_DECODE_SQL, &[])
        .await
        .expect("compio native byte/string decode");
    let decoded_bytea = [row.get(0), row.get(1)];
    let decoded_strings = [row.get(2), row.get(3), row.get(4), row.get(5), row.get(6)];
    let ascii_char_input = "xy".to_owned();
    let unicode_char_input = "\u{754c}\u{1f680}".to_owned();
    let rebound = client
        .query_one(
            NATIVE_BYTE_STRING_REBOUND_SQL,
            &[
                &decoded_bytea[0],
                &decoded_bytea[1],
                &decoded_strings[0],
                &decoded_strings[1],
                &decoded_strings[2],
                &ascii_char_input,
                &unicode_char_input,
            ],
        )
        .await
        .expect("compio native byte/string encode");
    NativeByteStringObservation {
        decoded_bytea,
        rebound_bytea: [rebound.get(0), rebound.get(1)],
        decoded_strings,
        rebound_strings: [
            rebound.get(2),
            rebound.get(3),
            rebound.get(4),
            rebound.get(5),
            rebound.get(6),
        ],
    }
}

/// Both drivers preserve byte and string values in text and binary formats.
#[compio::test]
async fn both_drivers_agree_on_byte_and_string_text_and_binary_codecs() {
    let cases = byte_string_format_cases();
    let theirs = tokio_byte_string_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_byte_string_format_observations(&cases).await;
    assert_eq!(ours, theirs);

    // BPCHAR's output function preserves its padding, while its cast to text
    // removes it. The generic invariant therefore applies through VARCHAR;
    // the two CHAR cases are checked against both server witnesses below.
    assert_format_differential(&cases[..5], &ours[..5], &theirs[..5]);

    let expected = [
        ("bytea-format-empty", "\\x", ""),
        ("bytea-format-high-bytes", "\\x00ff5c0a80c3", "00ff5c0a80c3"),
        ("text-format-empty", "", ""),
        (
            "text-format-hostile-utf8",
            "line one\r\nline two\t\\'\" / e\u{301} / \u{1f600} / \u{1f680} / \u{2028}",
            "6c696e65206f6e650d0a6c696e652074776f095c2722202f2065cc81202f20f09f9880202f20f09f9a80202f20e280a8",
        ),
        (
            "varchar-format-trailing-spaces",
            "varying  \u{754c}\u{1f680}  ",
            "76617279696e672020e7958cf09f9a802020",
        ),
    ];
    for (observation, (name, text, binary_hex)) in ours.iter().zip(expected) {
        assert_eq!(observation.name, name);
        assert_eq!(observation.text_decoded, text, "{name}: server text");
        assert_eq!(
            hex(&observation.binary_decoded.0),
            binary_hex,
            "{name}: wire"
        );
    }

    for (observation, padded, rendered, binary_hex) in [
        (&ours[5], "xy      ", "xy", "7879202020202020"),
        (
            &ours[6],
            "\u{754c}\u{1f680}  ",
            "\u{754c}\u{1f680}",
            "e7958cf09f9a802020",
        ),
    ] {
        assert_eq!(observation.text_decoded, padded);
        assert_eq!(observation.binary_decoded_text, rendered);
        assert_eq!(hex(&observation.binary_decoded.0), binary_hex);
        assert_eq!(observation.binary_decoded, observation.binary_encoded);
        assert_eq!(observation.binary_decoded, observation.text_encoded);
        assert_eq!(observation.binary_encoded_text, rendered);
        assert_eq!(observation.text_encoded_text, rendered);
    }

    let theirs = tokio_native_byte_string_observation(common::plaintext_url());
    let ours = compio_native_byte_string_observation().await;
    assert_eq!(ours, theirs);
    assert_eq!(
        ours.decoded_bytea,
        [Vec::new(), vec![0, 0xff, 0x5c, 0x0a, 0x80, 0xc3]]
    );
    assert_eq!(ours.decoded_bytea, ours.rebound_bytea);
    assert_eq!(
        ours.decoded_strings,
        [
            String::new(),
            "line one\r\nline two\t\\'\" / e\u{301} / \u{1f600} / \u{1f680} / \u{2028}".to_owned(),
            "varying  \u{754c}\u{1f680}  ".to_owned(),
            "xy      ".to_owned(),
            "\u{754c}\u{1f680}  ".to_owned(),
        ]
    );
    assert_eq!(ours.decoded_strings, ours.rebound_strings);
}

#[cfg(feature = "with-smol_str-01")]
const SMOL_STR_INLINE_23: &str = "abcdefghijklmnopqrstuvw";
#[cfg(feature = "with-smol_str-01")]
const SMOL_STR_HEAP_24: &str = "abcdefghijklmnopqrstuvwx";
#[cfg(feature = "with-smol_str-01")]
const SMOL_STR_DECOMPOSED_UNICODE: &str = "e\u{301}/\u{754c}/\u{1f680}";

#[cfg(feature = "with-smol_str-01")]
#[derive(Debug, PartialEq, Eq)]
struct NativeSmolStrObservation {
    decoded: [String; 5],
    decoded_heap_allocated: [bool; 5],
    server_text: [String; 5],
    server_wires: [Wire; 5],
    outbound: [String; 5],
    outbound_heap_allocated: [bool; 5],
    outbound_wires: [Vec<u8>; 5],
    rebound: [String; 5],
    rebound_heap_allocated: [bool; 5],
    rebound_text: [String; 5],
    rebound_wires: [Wire; 5],
}

#[cfg(feature = "with-smol_str-01")]
const NATIVE_SMOL_STR_DECODE_SQL: &str = "SELECT \
    ''::text, ''::text, (''::text)::text, \
    'abcdefghijklmnopqrstuvw'::text, 'abcdefghijklmnopqrstuvw'::text, \
        ('abcdefghijklmnopqrstuvw'::text)::text, \
    'abcdefghijklmnopqrstuvwx'::text, 'abcdefghijklmnopqrstuvwx'::text, \
        ('abcdefghijklmnopqrstuvwx'::text)::text, \
    $cpg$e\u{301}/\u{754c}/\u{1f680}$cpg$::text, \
        $cpg$e\u{301}/\u{754c}/\u{1f680}$cpg$::text, \
        ($cpg$e\u{301}/\u{754c}/\u{1f680}$cpg$::text)::text, \
    'xy'::char(5), 'xy'::char(5), ('xy'::char(5))::text";

#[cfg(feature = "with-smol_str-01")]
const NATIVE_SMOL_STR_REBOUND_SQL: &str = "SELECT \
    $1::text, $1::text, ($1::text)::text, \
    $2::text, $2::text, ($2::text)::text, \
    $3::text, $3::text, ($3::text)::text, \
    $4::text, $4::text, ($4::text)::text, \
    $5::char(5), $5::char(5), ($5::char(5))::text";

#[cfg(feature = "with-smol_str-01")]
fn smol_str_inputs() -> [smol_str::SmolStr; 5] {
    [
        smol_str::SmolStr::new(""),
        smol_str::SmolStr::new(SMOL_STR_INLINE_23),
        smol_str::SmolStr::new(SMOL_STR_HEAP_24),
        smol_str::SmolStr::new(SMOL_STR_DECOMPOSED_UNICODE),
        smol_str::SmolStr::new("xy"),
    ]
}

#[cfg(feature = "with-smol_str-01")]
fn smol_str_strings(values: &[smol_str::SmolStr; 5]) -> [String; 5] {
    values.each_ref().map(|value| value.as_str().to_owned())
}

#[cfg(feature = "with-smol_str-01")]
fn smol_str_heap_flags(values: &[smol_str::SmolStr; 5]) -> [bool; 5] {
    values.each_ref().map(|value| value.is_heap_allocated())
}

#[cfg(feature = "with-smol_str-01")]
fn tokio_smol_str_wire(value: &smol_str::SmolStr, ty: &tokio_types::Type) -> Vec<u8> {
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("tokio-postgres native SmolStr encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-smol_str-01")]
fn compio_smol_str_wire(value: &smol_str::SmolStr, ty: &compio_types::Type) -> Vec<u8> {
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("compio-postgres native SmolStr encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-smol_str-01")]
fn tokio_native_smol_str_observation(url: String) -> NativeSmolStrObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_SMOL_STR_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native SmolStr decode");
        let decoded = [row.get(0), row.get(3), row.get(6), row.get(9), row.get(12)];
        let outbound = smol_str_inputs();
        let outbound_wires = [
            tokio_smol_str_wire(&outbound[0], &tokio_types::Type::TEXT),
            tokio_smol_str_wire(&outbound[1], &tokio_types::Type::TEXT),
            tokio_smol_str_wire(&outbound[2], &tokio_types::Type::TEXT),
            tokio_smol_str_wire(&outbound[3], &tokio_types::Type::TEXT),
            tokio_smol_str_wire(&outbound[4], &tokio_types::Type::BPCHAR),
        ];
        let rebound = client
            .query_one(
                NATIVE_SMOL_STR_REBOUND_SQL,
                &[
                    &outbound[0],
                    &outbound[1],
                    &outbound[2],
                    &outbound[3],
                    &outbound[4],
                ],
            )
            .await
            .expect("tokio-postgres native SmolStr encode");
        let rebound_values = [
            rebound.get(0),
            rebound.get(3),
            rebound.get(6),
            rebound.get(9),
            rebound.get(12),
        ];

        NativeSmolStrObservation {
            decoded: smol_str_strings(&decoded),
            decoded_heap_allocated: smol_str_heap_flags(&decoded),
            server_text: [row.get(2), row.get(5), row.get(8), row.get(11), row.get(14)],
            server_wires: [row.get(1), row.get(4), row.get(7), row.get(10), row.get(13)],
            outbound: smol_str_strings(&outbound),
            outbound_heap_allocated: smol_str_heap_flags(&outbound),
            outbound_wires,
            rebound: smol_str_strings(&rebound_values),
            rebound_heap_allocated: smol_str_heap_flags(&rebound_values),
            rebound_text: [
                rebound.get(2),
                rebound.get(5),
                rebound.get(8),
                rebound.get(11),
                rebound.get(14),
            ],
            rebound_wires: [
                rebound.get(1),
                rebound.get(4),
                rebound.get(7),
                rebound.get(10),
                rebound.get(13),
            ],
        }
    })
}

#[cfg(feature = "with-smol_str-01")]
#[allow(clippy::future_not_send)]
async fn compio_native_smol_str_observation() -> NativeSmolStrObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_SMOL_STR_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native SmolStr decode");
    let decoded = [row.get(0), row.get(3), row.get(6), row.get(9), row.get(12)];
    let outbound = smol_str_inputs();
    let outbound_wires = [
        compio_smol_str_wire(&outbound[0], &compio_types::Type::TEXT),
        compio_smol_str_wire(&outbound[1], &compio_types::Type::TEXT),
        compio_smol_str_wire(&outbound[2], &compio_types::Type::TEXT),
        compio_smol_str_wire(&outbound[3], &compio_types::Type::TEXT),
        compio_smol_str_wire(&outbound[4], &compio_types::Type::BPCHAR),
    ];
    let rebound = client
        .query_one(
            NATIVE_SMOL_STR_REBOUND_SQL,
            &[
                &outbound[0],
                &outbound[1],
                &outbound[2],
                &outbound[3],
                &outbound[4],
            ],
        )
        .await
        .expect("compio-postgres native SmolStr encode");
    let rebound_values = [
        rebound.get(0),
        rebound.get(3),
        rebound.get(6),
        rebound.get(9),
        rebound.get(12),
    ];

    NativeSmolStrObservation {
        decoded: smol_str_strings(&decoded),
        decoded_heap_allocated: smol_str_heap_flags(&decoded),
        server_text: [row.get(2), row.get(5), row.get(8), row.get(11), row.get(14)],
        server_wires: [row.get(1), row.get(4), row.get(7), row.get(10), row.get(13)],
        outbound: smol_str_strings(&outbound),
        outbound_heap_allocated: smol_str_heap_flags(&outbound),
        outbound_wires,
        rebound: smol_str_strings(&rebound_values),
        rebound_heap_allocated: smol_str_heap_flags(&rebound_values),
        rebound_text: [
            rebound.get(2),
            rebound.get(5),
            rebound.get(8),
            rebound.get(11),
            rebound.get(14),
        ],
        rebound_wires: [
            rebound.get(1),
            rebound.get(4),
            rebound.get(7),
            rebound.get(10),
            rebound.get(13),
        ],
    }
}

/// Native `SmolStr` codecs preserve UTF-8 and the inline/heap boundary.
#[cfg(feature = "with-smol_str-01")]
#[compio::test]
async fn native_smol_str_codecs_cover_storage_boundary_and_char_padding() {
    let theirs = tokio_native_smol_str_observation(common::plaintext_url());
    let ours = compio_native_smol_str_observation().await;
    assert_eq!(ours, theirs);

    let expected_outbound = [
        "",
        SMOL_STR_INLINE_23,
        SMOL_STR_HEAP_24,
        SMOL_STR_DECOMPOSED_UNICODE,
        "xy",
    ];
    let expected_decoded = [
        "",
        SMOL_STR_INLINE_23,
        SMOL_STR_HEAP_24,
        SMOL_STR_DECOMPOSED_UNICODE,
        "xy   ",
    ];
    let expected_heap_allocated = [false, false, true, false, false];
    let expected_server_wires = [
        "",
        "6162636465666768696a6b6c6d6e6f7071727374757677",
        "6162636465666768696a6b6c6d6e6f707172737475767778",
        "65cc812fe7958c2ff09f9a80",
        "7879202020",
    ];
    let expected_outbound_wires = [
        "",
        "6162636465666768696a6b6c6d6e6f7071727374757677",
        "6162636465666768696a6b6c6d6e6f707172737475767778",
        "65cc812fe7958c2ff09f9a80",
        "7879",
    ];

    assert_eq!(ours.outbound, expected_outbound);
    assert_eq!(ours.decoded, expected_decoded);
    assert_eq!(ours.rebound, expected_decoded);
    assert_eq!(ours.server_text, expected_outbound);
    assert_eq!(ours.rebound_text, expected_outbound);
    assert_eq!(ours.outbound_heap_allocated, expected_heap_allocated);
    assert_eq!(ours.decoded_heap_allocated, expected_heap_allocated);
    assert_eq!(ours.rebound_heap_allocated, expected_heap_allocated);
    assert_eq!(
        ours.outbound.each_ref().map(|value| value.len()),
        [0, 23, 24, 12, 2]
    );
    assert_eq!(
        ours.decoded.each_ref().map(|value| value.len()),
        [0, 23, 24, 12, 5]
    );
    assert_eq!(
        ours.rebound.each_ref().map(|value| value.len()),
        [0, 23, 24, 12, 5]
    );
    for (index, expected_wire) in expected_server_wires.into_iter().enumerate() {
        assert_eq!(hex(&ours.server_wires[index].0), expected_wire);
        assert_eq!(hex(&ours.rebound_wires[index].0), expected_wire);
        assert_eq!(
            hex(&ours.outbound_wires[index]),
            expected_outbound_wires[index]
        );
    }
}

const JSON_KEY_ORDER_SOURCE: &str = r#"{"zz":0,"a":1,"bbb":2,"aa":3}"#;
const JSON_KEY_ORDER_JSONB_TEXT: &str = r#"{"a": 1, "aa": 3, "zz": 0, "bbb": 2}"#;
const JSON_UNICODE_SOURCE: &str =
    r#"{"bmp":"\u00e9\u754c","pair":"\uD83D\uDE80","solidus":"\/","control":"\u0001"}"#;
const JSON_UNICODE_JSONB_TEXT: &str =
    r#"{"bmp": "é界", "pair": "🚀", "control": "\u0001", "solidus": "/"}"#;

fn deep_json_source() -> String {
    format!(
        "{}{}{}",
        "[".repeat(64),
        r#"{"leaf":"\u754c"}"#,
        "]".repeat(64)
    )
}

fn deep_jsonb_text() -> String {
    format!(
        "{}{}{}",
        "[".repeat(64),
        r#"{"leaf": "界"}"#,
        "]".repeat(64)
    )
}

fn json_format_cases() -> Vec<RawCase> {
    let deep = deep_json_source();
    vec![
        RawCase::new(
            "json-format-key-order",
            &format!("$json${JSON_KEY_ORDER_SOURCE}$json$::json"),
            "json",
        ),
        RawCase::new(
            "jsonb-format-key-order",
            &format!("$json${JSON_KEY_ORDER_SOURCE}$json$::jsonb"),
            "jsonb",
        ),
        RawCase::new(
            "json-format-unicode-escapes",
            &format!("$json${JSON_UNICODE_SOURCE}$json$::json"),
            "json",
        ),
        RawCase::new(
            "jsonb-format-unicode-escapes",
            &format!("$json${JSON_UNICODE_SOURCE}$json$::jsonb"),
            "jsonb",
        ),
        RawCase::new(
            "json-format-deep-nesting",
            &format!("$json${deep}$json$::json"),
            "json",
        ),
        RawCase::new(
            "jsonb-format-deep-nesting",
            &format!("$json${deep}$json$::jsonb"),
            "jsonb",
        ),
    ]
}

fn tokio_json_format_observations(url: String, cases: Vec<RawCase>) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_json_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

/// Both drivers preserve JSON values through text and binary formats.
#[compio::test]
async fn both_drivers_agree_on_json_text_and_binary_codecs() {
    let cases = json_format_cases();
    let theirs = tokio_json_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_json_format_observations(&cases).await;
    assert_format_differential(&cases, &ours, &theirs);

    let deep_json = deep_json_source();
    let deep_jsonb = deep_jsonb_text();
    assert_eq!(deep_json.len(), 145, "deep JSON fixture changed shape");
    assert_eq!(deep_jsonb.len(), 143, "deep JSONB fixture changed shape");
    let expected = [
        ("json-format-key-order", JSON_KEY_ORDER_SOURCE, false),
        ("jsonb-format-key-order", JSON_KEY_ORDER_JSONB_TEXT, true),
        ("json-format-unicode-escapes", JSON_UNICODE_SOURCE, false),
        (
            "jsonb-format-unicode-escapes",
            JSON_UNICODE_JSONB_TEXT,
            true,
        ),
        ("json-format-deep-nesting", &deep_json, false),
        ("jsonb-format-deep-nesting", &deep_jsonb, true),
    ];
    for (observation, (name, text, is_jsonb)) in ours.iter().zip(expected) {
        assert_eq!(observation.name, name);
        assert_eq!(observation.text_decoded, text, "{name}: server text");
        let mut expected_wire = Vec::with_capacity(text.len() + usize::from(is_jsonb));
        if is_jsonb {
            expected_wire.push(1);
        }
        expected_wire.extend_from_slice(text.as_bytes());
        assert_eq!(observation.binary_decoded.0, expected_wire, "{name}: wire");
    }
}

fn extended_scalar_format_cases() -> Vec<RawCase> {
    vec![
        RawCase::new(
            "scalar-uuid",
            "'ffffffff-0000-8000-8000-0123456789ab'::uuid",
            "uuid",
        ),
        RawCase::new("scalar-inet-v4-prefix", "'192.0.2.129/24'::inet", "inet"),
        RawCase::new(
            "scalar-inet-v6-prefix",
            "'2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet",
            "inet",
        ),
        RawCase::new("scalar-cidr-v4", "'192.0.2.128/25'::cidr", "cidr"),
        RawCase::new("scalar-cidr-v6", "'2001:db8:abcd:ef00::/56'::cidr", "cidr"),
        RawCase::new("scalar-macaddr", "'08:00:2b:01:02:03'::macaddr", "macaddr"),
        RawCase::new("scalar-bit-nine", "B'101010101'::bit(9)", "bit(9)"),
        RawCase::new("scalar-varbit-empty", "B''::varbit", "varbit"),
        RawCase::new("scalar-varbit-nine", "B'101010101'::varbit(9)", "varbit(9)"),
        RawCase::new("scalar-oid-max", "4294967295::oid", "oid"),
        RawCase::new("scalar-money-max", "'92233720368547758.07'::money", "money"),
        RawCase::new("scalar-money-negative-cent", "'-0.01'::money", "money"),
    ]
}

fn tokio_extended_scalar_format_observations(
    url: String,
    cases: Vec<RawCase>,
) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_extended_scalar_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

#[derive(Debug, PartialEq, Eq)]
struct NativeExtendedScalarObservation {
    inet4_decoded: IpAddr,
    inet4_server_text: String,
    inet4_server_mask: i32,
    inet4_rebound: IpAddr,
    inet4_rebound_text: String,
    inet4_rebound_mask: i32,
    inet4_rebound_wire: Wire,
    inet6_decoded: IpAddr,
    inet6_server_text: String,
    inet6_server_mask: i32,
    inet6_rebound: IpAddr,
    inet6_rebound_text: String,
    inet6_rebound_mask: i32,
    inet6_rebound_wire: Wire,
    oid_decoded: u32,
    oid_rebound: u32,
}

const NATIVE_EXTENDED_SCALAR_DECODE_SQL: &str = "SELECT \
    '192.0.2.129/24'::inet, ('192.0.2.129/24'::inet)::text, \
        masklen('192.0.2.129/24'::inet), \
    '2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet, \
        ('2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet)::text, \
        masklen('2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet), \
    4294967295::oid";

const NATIVE_EXTENDED_SCALAR_REBOUND_SQL: &str = "SELECT \
    $1::inet, ($1::inet)::text, masklen($1::inet), $1::inet, \
    $2::inet, ($2::inet)::text, masklen($2::inet), $2::inet, \
    $3::oid";

fn tokio_native_extended_scalar_observation(url: String) -> NativeExtendedScalarObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_EXTENDED_SCALAR_DECODE_SQL, &[])
            .await
            .expect("tokio native extended-scalar decode");
        let inet4_decoded = row.get(0);
        let inet6_decoded = row.get(3);
        let oid_decoded = row.get(6);
        let rebound = client
            .query_one(
                NATIVE_EXTENDED_SCALAR_REBOUND_SQL,
                &[&inet4_decoded, &inet6_decoded, &oid_decoded],
            )
            .await
            .expect("tokio native extended-scalar encode");
        NativeExtendedScalarObservation {
            inet4_decoded,
            inet4_server_text: row.get(1),
            inet4_server_mask: row.get(2),
            inet4_rebound: rebound.get(0),
            inet4_rebound_text: rebound.get(1),
            inet4_rebound_mask: rebound.get(2),
            inet4_rebound_wire: rebound.get(3),
            inet6_decoded,
            inet6_server_text: row.get(4),
            inet6_server_mask: row.get(5),
            inet6_rebound: rebound.get(4),
            inet6_rebound_text: rebound.get(5),
            inet6_rebound_mask: rebound.get(6),
            inet6_rebound_wire: rebound.get(7),
            oid_decoded,
            oid_rebound: rebound.get(8),
        }
    })
}

#[allow(clippy::future_not_send)]
async fn compio_native_extended_scalar_observation() -> NativeExtendedScalarObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_EXTENDED_SCALAR_DECODE_SQL, &[])
        .await
        .expect("compio native extended-scalar decode");
    let inet4_decoded = row.get(0);
    let inet6_decoded = row.get(3);
    let oid_decoded = row.get(6);
    let rebound = client
        .query_one(
            NATIVE_EXTENDED_SCALAR_REBOUND_SQL,
            &[&inet4_decoded, &inet6_decoded, &oid_decoded],
        )
        .await
        .expect("compio native extended-scalar encode");
    NativeExtendedScalarObservation {
        inet4_decoded,
        inet4_server_text: row.get(1),
        inet4_server_mask: row.get(2),
        inet4_rebound: rebound.get(0),
        inet4_rebound_text: rebound.get(1),
        inet4_rebound_mask: rebound.get(2),
        inet4_rebound_wire: rebound.get(3),
        inet6_decoded,
        inet6_server_text: row.get(4),
        inet6_server_mask: row.get(5),
        inet6_rebound: rebound.get(4),
        inet6_rebound_text: rebound.get(5),
        inet6_rebound_mask: rebound.get(6),
        inet6_rebound_wire: rebound.get(7),
        oid_decoded,
        oid_rebound: rebound.get(8),
    }
}

/// Supported extended scalars agree in text and binary formats.
#[compio::test]
async fn both_drivers_agree_on_extended_scalar_text_and_binary_codecs() {
    let cases = extended_scalar_format_cases();
    let theirs = tokio_extended_scalar_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_extended_scalar_format_observations(&cases).await;
    assert_format_differential(&cases, &ours, &theirs);

    let expected = [
        (
            "scalar-uuid",
            "ffffffff-0000-8000-8000-0123456789ab",
            "ffffffff0000800080000123456789ab",
        ),
        (
            "scalar-inet-v4-prefix",
            "192.0.2.129/24",
            "02180004c0000281",
        ),
        (
            "scalar-inet-v6-prefix",
            "2001:db8:abcd:ef01:2345:6789:abcd:ef01/73",
            "0349001020010db8abcdef0123456789abcdef01",
        ),
        ("scalar-cidr-v4", "192.0.2.128/25", "02190104c0000280"),
        (
            "scalar-cidr-v6",
            "2001:db8:abcd:ef00::/56",
            "0338011020010db8abcdef000000000000000000",
        ),
        ("scalar-macaddr", "08:00:2b:01:02:03", "08002b010203"),
        ("scalar-bit-nine", "101010101", "00000009aa80"),
        ("scalar-varbit-empty", "", "00000000"),
        ("scalar-varbit-nine", "101010101", "00000009aa80"),
        ("scalar-oid-max", "4294967295", "ffffffff"),
        (
            "scalar-money-max",
            "$92,233,720,368,547,758.07",
            "7fffffffffffffff",
        ),
        ("scalar-money-negative-cent", "-$0.01", "ffffffffffffffff"),
    ];
    for (observation, (name, text, binary_hex)) in ours.iter().zip(expected) {
        assert_eq!(observation.name, name);
        assert_eq!(observation.text_decoded, text, "{name}: server text");
        assert_eq!(
            hex(&observation.binary_decoded.0),
            binary_hex,
            "{name}: wire"
        );
    }

    let theirs = tokio_native_extended_scalar_observation(common::plaintext_url());
    let ours = compio_native_extended_scalar_observation().await;
    assert_eq!(ours, theirs);
    assert_eq!(ours.oid_decoded, u32::MAX);
    assert_eq!(ours.oid_rebound, u32::MAX);

    assert_eq!(ours.inet4_decoded, "192.0.2.129".parse::<IpAddr>().unwrap());
    assert_eq!(ours.inet4_rebound, ours.inet4_decoded);
    assert_eq!(ours.inet4_server_text, "192.0.2.129/24");
    assert_eq!(ours.inet4_server_mask, 24);
    assert_eq!(ours.inet4_rebound_text, "192.0.2.129/32");
    assert_eq!(ours.inet4_rebound_mask, 32);
    assert_eq!(hex(&ours.inet4_rebound_wire.0), "02200004c0000281");

    assert_eq!(
        ours.inet6_decoded,
        "2001:db8:abcd:ef01:2345:6789:abcd:ef01"
            .parse::<IpAddr>()
            .unwrap()
    );
    assert_eq!(ours.inet6_rebound, ours.inet6_decoded);
    assert_eq!(
        ours.inet6_server_text,
        "2001:db8:abcd:ef01:2345:6789:abcd:ef01/73"
    );
    assert_eq!(ours.inet6_server_mask, 73);
    assert_eq!(
        ours.inet6_rebound_text,
        "2001:db8:abcd:ef01:2345:6789:abcd:ef01/128"
    );
    assert_eq!(ours.inet6_rebound_mask, 128);
    assert_eq!(
        hex(&ours.inet6_rebound_wire.0),
        "0380001020010db8abcdef0123456789abcdef01"
    );
}

/// Desired invariant blocked by both `IpAddr` codecs discarding INET prefixes.
#[ignore = "both postgres-types IpAddr codecs discard INET prefix lengths"]
#[compio::test]
async fn native_inet_codecs_must_not_discard_prefix_lengths() {
    let theirs = tokio_native_extended_scalar_observation(common::plaintext_url());
    let ours = compio_native_extended_scalar_observation().await;
    assert_eq!(ours.inet4_server_mask, ours.inet4_rebound_mask);
    assert_eq!(theirs.inet4_server_mask, theirs.inet4_rebound_mask);
    assert_eq!(ours.inet6_server_mask, ours.inet6_rebound_mask);
    assert_eq!(theirs.inet6_server_mask, theirs.inet6_rebound_mask);
}

#[cfg(feature = "with-cidr-0_3")]
#[derive(Debug, PartialEq, Eq)]
struct NativeCidrObservation {
    decoded_text: [String; 4],
    decoded_masks: [u8; 4],
    server_text: [String; 4],
    server_masks: [i32; 4],
    server_wires: [Wire; 4],
    outbound_wires: [Vec<u8>; 4],
    rebound_text: [String; 4],
    rebound_masks: [i32; 4],
    rebound_wires: [Wire; 4],
}

#[cfg(feature = "with-cidr-0_3")]
const NATIVE_CIDR_DECODE_SQL: &str = "SELECT \
    '192.0.2.128/25'::cidr, '192.0.2.128/25'::cidr, \
        ('192.0.2.128/25'::cidr)::text, masklen('192.0.2.128/25'::cidr), \
    '2001:db8:abcd:ef00::/56'::cidr, '2001:db8:abcd:ef00::/56'::cidr, \
        ('2001:db8:abcd:ef00::/56'::cidr)::text, \
        masklen('2001:db8:abcd:ef00::/56'::cidr), \
    '192.0.2.129/24'::inet, '192.0.2.129/24'::inet, \
        ('192.0.2.129/24'::inet)::text, masklen('192.0.2.129/24'::inet), \
    '2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet, \
        '2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet, \
        ('2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet)::text, \
        masklen('2001:db8:abcd:ef01:2345:6789:abcd:ef01/73'::inet)";

#[cfg(feature = "with-cidr-0_3")]
const NATIVE_CIDR_REBOUND_SQL: &str = "SELECT \
    ($1::cidr)::text, masklen($1::cidr), $1::cidr, \
    ($2::cidr)::text, masklen($2::cidr), $2::cidr, \
    ($3::inet)::text, masklen($3::inet), $3::inet, \
    ($4::inet)::text, masklen($4::inet), $4::inet";

#[cfg(feature = "with-cidr-0_3")]
fn tokio_native_wire<T>(value: &T, ty: &tokio_types::Type) -> Vec<u8>
where
    T: tokio_types::ToSql,
{
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql(value, ty, &mut wire)
        .expect("tokio-postgres native network encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-cidr-0_3")]
fn compio_native_wire<T>(value: &T, ty: &compio_types::Type) -> Vec<u8>
where
    T: compio_types::ToSql,
{
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql(value, ty, &mut wire)
        .expect("compio-postgres native network encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-cidr-0_3")]
fn tokio_native_cidr_observation(url: String) -> NativeCidrObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_CIDR_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native CIDR decode");
        let cidr4: cidr::IpCidr = row.get(0);
        let cidr6: cidr::IpCidr = row.get(4);
        let inet4: cidr::IpInet = row.get(8);
        let inet6: cidr::IpInet = row.get(12);
        let outbound_wires = [
            tokio_native_wire(&cidr4, &tokio_types::Type::CIDR),
            tokio_native_wire(&cidr6, &tokio_types::Type::CIDR),
            tokio_native_wire(&inet4, &tokio_types::Type::INET),
            tokio_native_wire(&inet6, &tokio_types::Type::INET),
        ];
        let rebound = client
            .query_one(NATIVE_CIDR_REBOUND_SQL, &[&cidr4, &cidr6, &inet4, &inet6])
            .await
            .expect("tokio-postgres native CIDR encode");

        NativeCidrObservation {
            decoded_text: [
                cidr4.to_string(),
                cidr6.to_string(),
                inet4.to_string(),
                inet6.to_string(),
            ],
            decoded_masks: [
                cidr4.network_length(),
                cidr6.network_length(),
                inet4.network_length(),
                inet6.network_length(),
            ],
            server_text: [row.get(2), row.get(6), row.get(10), row.get(14)],
            server_masks: [row.get(3), row.get(7), row.get(11), row.get(15)],
            server_wires: [row.get(1), row.get(5), row.get(9), row.get(13)],
            outbound_wires,
            rebound_text: [
                rebound.get(0),
                rebound.get(3),
                rebound.get(6),
                rebound.get(9),
            ],
            rebound_masks: [
                rebound.get(1),
                rebound.get(4),
                rebound.get(7),
                rebound.get(10),
            ],
            rebound_wires: [
                rebound.get(2),
                rebound.get(5),
                rebound.get(8),
                rebound.get(11),
            ],
        }
    })
}

#[cfg(feature = "with-cidr-0_3")]
#[allow(clippy::future_not_send)]
async fn compio_native_cidr_observation() -> NativeCidrObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_CIDR_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native CIDR decode");
    let cidr4: cidr::IpCidr = row.get(0);
    let cidr6: cidr::IpCidr = row.get(4);
    let inet4: cidr::IpInet = row.get(8);
    let inet6: cidr::IpInet = row.get(12);
    let outbound_wires = [
        compio_native_wire(&cidr4, &compio_types::Type::CIDR),
        compio_native_wire(&cidr6, &compio_types::Type::CIDR),
        compio_native_wire(&inet4, &compio_types::Type::INET),
        compio_native_wire(&inet6, &compio_types::Type::INET),
    ];
    let rebound = client
        .query_one(NATIVE_CIDR_REBOUND_SQL, &[&cidr4, &cidr6, &inet4, &inet6])
        .await
        .expect("compio-postgres native CIDR encode");

    NativeCidrObservation {
        decoded_text: [
            cidr4.to_string(),
            cidr6.to_string(),
            inet4.to_string(),
            inet6.to_string(),
        ],
        decoded_masks: [
            cidr4.network_length(),
            cidr6.network_length(),
            inet4.network_length(),
            inet6.network_length(),
        ],
        server_text: [row.get(2), row.get(6), row.get(10), row.get(14)],
        server_masks: [row.get(3), row.get(7), row.get(11), row.get(15)],
        server_wires: [row.get(1), row.get(5), row.get(9), row.get(13)],
        outbound_wires,
        rebound_text: [
            rebound.get(0),
            rebound.get(3),
            rebound.get(6),
            rebound.get(9),
        ],
        rebound_masks: [
            rebound.get(1),
            rebound.get(4),
            rebound.get(7),
            rebound.get(10),
        ],
        rebound_wires: [
            rebound.get(2),
            rebound.get(5),
            rebound.get(8),
            rebound.get(11),
        ],
    }
}

/// Native CIDR carriers retain prefixes. The local encoder also emits the
/// server's `is_cidr=1`; upstream emits `0`, which `PostgreSQL` accepts and then
/// normalizes, so an ordinary round trip would hide that wire defect.
#[cfg(feature = "with-cidr-0_3")]
#[compio::test]
async fn native_cidr_codecs_preserve_prefixes_and_expose_upstream_flag_defect() {
    let theirs = tokio_native_cidr_observation(common::plaintext_url());
    let ours = compio_native_cidr_observation().await;

    assert_eq!(ours.decoded_text, theirs.decoded_text);
    assert_eq!(ours.decoded_masks, theirs.decoded_masks);
    assert_eq!(ours.server_text, theirs.server_text);
    assert_eq!(ours.server_masks, theirs.server_masks);
    assert_eq!(ours.server_wires, theirs.server_wires);
    assert_eq!(ours.rebound_text, theirs.rebound_text);
    assert_eq!(ours.rebound_masks, theirs.rebound_masks);
    assert_eq!(ours.rebound_wires, theirs.rebound_wires);

    let expected_text = [
        "192.0.2.128/25",
        "2001:db8:abcd:ef00::/56",
        "192.0.2.129/24",
        "2001:db8:abcd:ef01:2345:6789:abcd:ef01/73",
    ];
    assert_eq!(ours.decoded_text, expected_text);
    assert_eq!(ours.server_text, expected_text);
    assert_eq!(ours.rebound_text, expected_text);
    assert_eq!(ours.decoded_masks, [25, 56, 24, 73]);
    assert_eq!(ours.server_masks, [25, 56, 24, 73]);
    assert_eq!(ours.rebound_masks, [25, 56, 24, 73]);

    let server_wire = [
        "02190104c0000280",
        "0338011020010db8abcdef000000000000000000",
        "02180004c0000281",
        "0349001020010db8abcdef0123456789abcdef01",
    ];
    let upstream_wire = [
        "02190004c0000280",
        "0338001020010db8abcdef000000000000000000",
        server_wire[2],
        server_wire[3],
    ];
    for index in 0..server_wire.len() {
        assert_eq!(hex(&ours.server_wires[index].0), server_wire[index]);
        assert_eq!(hex(&ours.rebound_wires[index].0), server_wire[index]);
        assert_eq!(hex(&ours.outbound_wires[index]), server_wire[index]);
        assert_eq!(hex(&theirs.outbound_wires[index]), upstream_wire[index]);
    }
    assert_ne!(theirs.outbound_wires[0], theirs.server_wires[0].0);
    assert_ne!(theirs.outbound_wires[1], theirs.server_wires[1].0);
}

#[cfg(feature = "with-eui48-1")]
#[derive(Debug, PartialEq, Eq)]
struct NativeEui48Observation {
    decoded: [[u8; 6]; 3],
    server_text: [String; 3],
    server_wires: [Wire; 3],
    outbound_wires: [Vec<u8>; 3],
    rebound: [[u8; 6]; 3],
    rebound_text: [String; 3],
    rebound_wires: [Wire; 3],
    macaddr8_text: [String; 2],
    macaddr8_wires: [Wire; 2],
    macaddr8_encode_supported: bool,
    macaddr8_decode_supported: bool,
}

#[cfg(feature = "with-eui48-1")]
const NATIVE_EUI48_DECODE_SQL: &str = "SELECT \
    '00:00:00:00:00:00'::macaddr, '00:00:00:00:00:00'::macaddr, \
        ('00:00:00:00:00:00'::macaddr)::text, \
    'ff:ff:ff:ff:ff:ff'::macaddr, 'ff:ff:ff:ff:ff:ff'::macaddr, \
        ('ff:ff:ff:ff:ff:ff'::macaddr)::text, \
    '08:00:2b:01:02:03'::macaddr, '08:00:2b:01:02:03'::macaddr, \
        ('08:00:2b:01:02:03'::macaddr)::text, \
    '08:00:2b:01:02:03:04:05'::macaddr8, \
        ('08:00:2b:01:02:03:04:05'::macaddr8)::text, \
    ('08:00:2b:01:02:03'::macaddr)::macaddr8, \
        (('08:00:2b:01:02:03'::macaddr)::macaddr8)::text";

#[cfg(feature = "with-eui48-1")]
const NATIVE_EUI48_REBOUND_SQL: &str = "SELECT \
    $1::macaddr, $1::macaddr, ($1::macaddr)::text, \
    $2::macaddr, $2::macaddr, ($2::macaddr)::text, \
    $3::macaddr, $3::macaddr, ($3::macaddr)::text";

#[cfg(feature = "with-eui48-1")]
fn tokio_eui48_wire(value: eui48::MacAddress, ty: &tokio_types::Type) -> Result<Vec<u8>, String> {
    let mut wire = tokio_types::private::BytesMut::new();
    match tokio_types::ToSql::to_sql_checked(&value, ty, &mut wire) {
        Ok(tokio_types::IsNull::No) => Ok(wire.to_vec()),
        Ok(tokio_types::IsNull::Yes) => Err("unexpected NULL".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(feature = "with-eui48-1")]
fn compio_eui48_wire(value: eui48::MacAddress, ty: &compio_types::Type) -> Result<Vec<u8>, String> {
    let mut wire = compio_types::private::BytesMut::new();
    match compio_types::ToSql::to_sql_checked(&value, ty, &mut wire) {
        Ok(compio_types::IsNull::No) => Ok(wire.to_vec()),
        Ok(compio_types::IsNull::Yes) => Err("unexpected NULL".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(feature = "with-eui48-1")]
fn tokio_native_eui48_observation(url: String) -> NativeEui48Observation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_EUI48_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native EUI-48 decode");
        let nil: eui48::MacAddress = row.get(0);
        let broadcast: eui48::MacAddress = row.get(3);
        let pattern: eui48::MacAddress = row.get(6);
        let outbound_wires = [
            tokio_eui48_wire(nil, &tokio_types::Type::MACADDR)
                .expect("tokio-postgres nil EUI-48 encode"),
            tokio_eui48_wire(broadcast, &tokio_types::Type::MACADDR)
                .expect("tokio-postgres broadcast EUI-48 encode"),
            tokio_eui48_wire(pattern, &tokio_types::Type::MACADDR)
                .expect("tokio-postgres patterned EUI-48 encode"),
        ];
        let rebound = client
            .query_one(NATIVE_EUI48_REBOUND_SQL, &[&nil, &broadcast, &pattern])
            .await
            .expect("tokio-postgres native EUI-48 encode");

        NativeEui48Observation {
            decoded: [nil.to_array(), broadcast.to_array(), pattern.to_array()],
            server_text: [row.get(2), row.get(5), row.get(8)],
            server_wires: [row.get(1), row.get(4), row.get(7)],
            outbound_wires,
            rebound: [
                rebound.get::<_, eui48::MacAddress>(0).to_array(),
                rebound.get::<_, eui48::MacAddress>(3).to_array(),
                rebound.get::<_, eui48::MacAddress>(6).to_array(),
            ],
            rebound_text: [rebound.get(2), rebound.get(5), rebound.get(8)],
            rebound_wires: [rebound.get(1), rebound.get(4), rebound.get(7)],
            macaddr8_text: [row.get(10), row.get(12)],
            macaddr8_wires: [row.get(9), row.get(11)],
            macaddr8_encode_supported: tokio_eui48_wire(pattern, &tokio_types::Type::MACADDR8)
                .is_ok(),
            macaddr8_decode_supported: row.try_get::<_, eui48::MacAddress>(9).is_ok(),
        }
    })
}

#[cfg(feature = "with-eui48-1")]
#[allow(clippy::future_not_send)]
async fn compio_native_eui48_observation() -> NativeEui48Observation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_EUI48_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native EUI-48 decode");
    let nil: eui48::MacAddress = row.get(0);
    let broadcast: eui48::MacAddress = row.get(3);
    let pattern: eui48::MacAddress = row.get(6);
    let outbound_wires = [
        compio_eui48_wire(nil, &compio_types::Type::MACADDR)
            .expect("compio-postgres nil EUI-48 encode"),
        compio_eui48_wire(broadcast, &compio_types::Type::MACADDR)
            .expect("compio-postgres broadcast EUI-48 encode"),
        compio_eui48_wire(pattern, &compio_types::Type::MACADDR)
            .expect("compio-postgres patterned EUI-48 encode"),
    ];
    let rebound = client
        .query_one(NATIVE_EUI48_REBOUND_SQL, &[&nil, &broadcast, &pattern])
        .await
        .expect("compio-postgres native EUI-48 encode");

    NativeEui48Observation {
        decoded: [nil.to_array(), broadcast.to_array(), pattern.to_array()],
        server_text: [row.get(2), row.get(5), row.get(8)],
        server_wires: [row.get(1), row.get(4), row.get(7)],
        outbound_wires,
        rebound: [
            rebound.get::<_, eui48::MacAddress>(0).to_array(),
            rebound.get::<_, eui48::MacAddress>(3).to_array(),
            rebound.get::<_, eui48::MacAddress>(6).to_array(),
        ],
        rebound_text: [rebound.get(2), rebound.get(5), rebound.get(8)],
        rebound_wires: [rebound.get(1), rebound.get(4), rebound.get(7)],
        macaddr8_text: [row.get(10), row.get(12)],
        macaddr8_wires: [row.get(9), row.get(11)],
        macaddr8_encode_supported: compio_eui48_wire(pattern, &compio_types::Type::MACADDR8)
            .is_ok(),
        macaddr8_decode_supported: row.try_get::<_, eui48::MacAddress>(9).is_ok(),
    }
}

/// The six-byte native carrier agrees with the server. `macaddr8` remains a
/// distinct eight-byte type that neither native codec accepts.
#[cfg(feature = "with-eui48-1")]
#[compio::test]
async fn native_eui48_codecs_cover_macaddr_and_expose_macaddr8_limit() {
    let theirs = tokio_native_eui48_observation(common::plaintext_url());
    let ours = compio_native_eui48_observation().await;
    assert_eq!(ours, theirs);

    let expected_bytes = [
        [0, 0, 0, 0, 0, 0],
        [0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03],
    ];
    let expected_text = [
        "00:00:00:00:00:00",
        "ff:ff:ff:ff:ff:ff",
        "08:00:2b:01:02:03",
    ];
    let expected_wires = ["000000000000", "ffffffffffff", "08002b010203"];
    assert_eq!(ours.decoded, expected_bytes);
    assert_eq!(ours.rebound, expected_bytes);
    assert_eq!(ours.server_text, expected_text);
    assert_eq!(ours.rebound_text, expected_text);
    for (index, expected_wire) in expected_wires.into_iter().enumerate() {
        assert_eq!(hex(&ours.server_wires[index].0), expected_wire);
        assert_eq!(hex(&ours.outbound_wires[index]), expected_wire);
        assert_eq!(hex(&ours.rebound_wires[index].0), expected_wire);
    }

    assert_eq!(
        ours.macaddr8_text,
        ["08:00:2b:01:02:03:04:05", "08:00:2b:ff:fe:01:02:03"]
    );
    assert_eq!(
        ours.macaddr8_wires.each_ref().map(|wire| hex(&wire.0)),
        ["08002b0102030405", "08002bfffe010203"]
    );
    assert!(!ours.macaddr8_encode_supported);
    assert!(!ours.macaddr8_decode_supported);
}

/// Desired invariant blocked by the six-byte-only `eui48::MacAddress` carrier.
#[cfg(feature = "with-eui48-1")]
#[ignore = "both eui48::MacAddress codecs reject PostgreSQL MACADDR8"]
#[compio::test]
async fn native_eui48_codecs_must_support_macaddr8() {
    let theirs = tokio_native_eui48_observation(common::plaintext_url());
    let ours = compio_native_eui48_observation().await;
    assert!(ours.macaddr8_encode_supported);
    assert!(ours.macaddr8_decode_supported);
    assert!(theirs.macaddr8_encode_supported);
    assert!(theirs.macaddr8_decode_supported);
}

#[cfg(feature = "with-bit-vec-0_9")]
#[derive(Debug, PartialEq, Eq)]
struct NativeBitVecObservation {
    decoded_bits: [Vec<bool>; 3],
    server_text: [String; 3],
    server_wires: [Wire; 3],
    outbound_wires: [Vec<u8>; 3],
    rebound_bits: [Vec<bool>; 3],
    rebound_text: [String; 3],
    rebound_wires: [Wire; 3],
}

#[cfg(feature = "with-bit-vec-0_9")]
const NATIVE_BIT_VEC_DECODE_SQL: &str = "SELECT \
    B''::varbit, B''::varbit, (B''::varbit)::text, \
    B'1'::bit(1), B'1'::bit(1), (B'1'::bit(1))::text, \
    B'1011001110001'::varbit, B'1011001110001'::varbit, \
        (B'1011001110001'::varbit)::text";

#[cfg(feature = "with-bit-vec-0_9")]
const NATIVE_BIT_VEC_REBOUND_SQL: &str = "SELECT \
    $1::varbit, $1::varbit, ($1::varbit)::text, \
    $2::bit(1), $2::bit(1), ($2::bit(1))::text, \
    $3::varbit, $3::varbit, ($3::varbit)::text";

#[cfg(feature = "with-bit-vec-0_9")]
fn bit_vec_bits(value: &bit_vec::BitVec) -> Vec<bool> {
    value.iter().collect()
}

#[cfg(feature = "with-bit-vec-0_9")]
fn tokio_bit_vec_wire(value: &bit_vec::BitVec, ty: &tokio_types::Type) -> Vec<u8> {
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("tokio-postgres native bit-vector encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-bit-vec-0_9")]
fn compio_bit_vec_wire(value: &bit_vec::BitVec, ty: &compio_types::Type) -> Vec<u8> {
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("compio-postgres native bit-vector encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-bit-vec-0_9")]
fn tokio_native_bit_vec_observation(url: String) -> NativeBitVecObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_BIT_VEC_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native bit-vector decode");
        let empty: bit_vec::BitVec = row.get(0);
        let one: bit_vec::BitVec = row.get(3);
        let thirteen: bit_vec::BitVec = row.get(6);
        let outbound_wires = [
            tokio_bit_vec_wire(&empty, &tokio_types::Type::VARBIT),
            tokio_bit_vec_wire(&one, &tokio_types::Type::BIT),
            tokio_bit_vec_wire(&thirteen, &tokio_types::Type::VARBIT),
        ];
        let rebound = client
            .query_one(NATIVE_BIT_VEC_REBOUND_SQL, &[&empty, &one, &thirteen])
            .await
            .expect("tokio-postgres native bit-vector encode");

        NativeBitVecObservation {
            decoded_bits: [
                bit_vec_bits(&empty),
                bit_vec_bits(&one),
                bit_vec_bits(&thirteen),
            ],
            server_text: [row.get(2), row.get(5), row.get(8)],
            server_wires: [row.get(1), row.get(4), row.get(7)],
            outbound_wires,
            rebound_bits: [
                bit_vec_bits(&rebound.get(0)),
                bit_vec_bits(&rebound.get(3)),
                bit_vec_bits(&rebound.get(6)),
            ],
            rebound_text: [rebound.get(2), rebound.get(5), rebound.get(8)],
            rebound_wires: [rebound.get(1), rebound.get(4), rebound.get(7)],
        }
    })
}

#[cfg(feature = "with-bit-vec-0_9")]
#[allow(clippy::future_not_send)]
async fn compio_native_bit_vec_observation() -> NativeBitVecObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_BIT_VEC_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native bit-vector decode");
    let empty: bit_vec::BitVec = row.get(0);
    let one: bit_vec::BitVec = row.get(3);
    let thirteen: bit_vec::BitVec = row.get(6);
    let outbound_wires = [
        compio_bit_vec_wire(&empty, &compio_types::Type::VARBIT),
        compio_bit_vec_wire(&one, &compio_types::Type::BIT),
        compio_bit_vec_wire(&thirteen, &compio_types::Type::VARBIT),
    ];
    let rebound = client
        .query_one(NATIVE_BIT_VEC_REBOUND_SQL, &[&empty, &one, &thirteen])
        .await
        .expect("compio-postgres native bit-vector encode");

    NativeBitVecObservation {
        decoded_bits: [
            bit_vec_bits(&empty),
            bit_vec_bits(&one),
            bit_vec_bits(&thirteen),
        ],
        server_text: [row.get(2), row.get(5), row.get(8)],
        server_wires: [row.get(1), row.get(4), row.get(7)],
        outbound_wires,
        rebound_bits: [
            bit_vec_bits(&rebound.get(0)),
            bit_vec_bits(&rebound.get(3)),
            bit_vec_bits(&rebound.get(6)),
        ],
        rebound_text: [rebound.get(2), rebound.get(5), rebound.get(8)],
        rebound_wires: [rebound.get(1), rebound.get(4), rebound.get(7)],
    }
}

/// Native bit-vector codecs retain exact bit lengths, encode the declared bit
/// count, and clear the unused low bits of the last wire byte.
#[cfg(feature = "with-bit-vec-0_9")]
#[compio::test]
async fn native_bit_vec_codecs_cover_lengths_and_final_byte_padding() {
    let theirs = tokio_native_bit_vec_observation(common::plaintext_url());
    let ours = compio_native_bit_vec_observation().await;
    assert_eq!(ours, theirs);

    let expected_bits = [
        vec![],
        vec![true],
        vec![
            true, false, true, true, false, false, true, true, true, false, false, false, true,
        ],
    ];
    let expected_text = ["", "1", "1011001110001"];
    let expected_wires = ["00000000", "0000000180", "0000000db388"];
    let expected_wire_lengths = [4, 5, 6];
    assert_eq!(ours.decoded_bits, expected_bits);
    assert_eq!(ours.rebound_bits, expected_bits);
    assert_eq!(ours.server_text, expected_text);
    assert_eq!(ours.rebound_text, expected_text);
    for (index, expected_wire) in expected_wires.into_iter().enumerate() {
        assert_eq!(
            ours.server_wires[index].0.len(),
            expected_wire_lengths[index]
        );
        assert_eq!(
            ours.outbound_wires[index].len(),
            expected_wire_lengths[index]
        );
        assert_eq!(
            ours.rebound_wires[index].0.len(),
            expected_wire_lengths[index]
        );
        assert_eq!(hex(&ours.server_wires[index].0), expected_wire);
        assert_eq!(hex(&ours.outbound_wires[index]), expected_wire);
        assert_eq!(hex(&ours.rebound_wires[index].0), expected_wire);
    }
    assert_eq!(ours.outbound_wires[2][5] & 0b0000_0111, 0);
}

#[cfg(feature = "with-uuid-1")]
#[derive(Debug, PartialEq, Eq)]
struct NativeUuidObservation {
    decoded: [uuid::Uuid; 3],
    server_text: [String; 3],
    server_wires: [Wire; 3],
    outbound_wires: [Vec<u8>; 3],
    rebound: [uuid::Uuid; 3],
    rebound_text: [String; 3],
    rebound_wires: [Wire; 3],
}

#[cfg(feature = "with-uuid-1")]
const NATIVE_UUID_DECODE_SQL: &str = "SELECT \
    '00000000-0000-0000-0000-000000000000'::uuid, \
        '00000000-0000-0000-0000-000000000000'::uuid, \
        ('00000000-0000-0000-0000-000000000000'::uuid)::text, \
    'ffffffff-ffff-ffff-ffff-ffffffffffff'::uuid, \
        'ffffffff-ffff-ffff-ffff-ffffffffffff'::uuid, \
        ('ffffffff-ffff-ffff-ffff-ffffffffffff'::uuid)::text, \
    'f81d4fae-7dec-4a0c-a765-00a0c91e6bf6'::uuid, \
        'f81d4fae-7dec-4a0c-a765-00a0c91e6bf6'::uuid, \
        ('f81d4fae-7dec-4a0c-a765-00a0c91e6bf6'::uuid)::text";

#[cfg(feature = "with-uuid-1")]
const NATIVE_UUID_REBOUND_SQL: &str = "SELECT \
    $1::uuid, $1::uuid, ($1::uuid)::text, \
    $2::uuid, $2::uuid, ($2::uuid)::text, \
    $3::uuid, $3::uuid, ($3::uuid)::text";

#[cfg(feature = "with-uuid-1")]
fn tokio_uuid_wire(value: &uuid::Uuid) -> Vec<u8> {
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, &tokio_types::Type::UUID, &mut wire)
        .expect("tokio-postgres native UUID encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-uuid-1")]
fn compio_uuid_wire(value: &uuid::Uuid) -> Vec<u8> {
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, &compio_types::Type::UUID, &mut wire)
        .expect("compio-postgres native UUID encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-uuid-1")]
fn tokio_native_uuid_observation(url: String) -> NativeUuidObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_UUID_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native UUID decode");
        let decoded = [row.get(0), row.get(3), row.get(6)];
        let outbound_wires = decoded.each_ref().map(tokio_uuid_wire);
        let rebound = client
            .query_one(
                NATIVE_UUID_REBOUND_SQL,
                &[&decoded[0], &decoded[1], &decoded[2]],
            )
            .await
            .expect("tokio-postgres native UUID encode");

        NativeUuidObservation {
            decoded,
            server_text: [row.get(2), row.get(5), row.get(8)],
            server_wires: [row.get(1), row.get(4), row.get(7)],
            outbound_wires,
            rebound: [rebound.get(0), rebound.get(3), rebound.get(6)],
            rebound_text: [rebound.get(2), rebound.get(5), rebound.get(8)],
            rebound_wires: [rebound.get(1), rebound.get(4), rebound.get(7)],
        }
    })
}

#[cfg(feature = "with-uuid-1")]
#[allow(clippy::future_not_send)]
async fn compio_native_uuid_observation() -> NativeUuidObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_UUID_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native UUID decode");
    let decoded = [row.get(0), row.get(3), row.get(6)];
    let outbound_wires = decoded.each_ref().map(compio_uuid_wire);
    let rebound = client
        .query_one(
            NATIVE_UUID_REBOUND_SQL,
            &[&decoded[0], &decoded[1], &decoded[2]],
        )
        .await
        .expect("compio-postgres native UUID encode");

    NativeUuidObservation {
        decoded,
        server_text: [row.get(2), row.get(5), row.get(8)],
        server_wires: [row.get(1), row.get(4), row.get(7)],
        outbound_wires,
        rebound: [rebound.get(0), rebound.get(3), rebound.get(6)],
        rebound_text: [rebound.get(2), rebound.get(5), rebound.get(8)],
        rebound_wires: [rebound.get(1), rebound.get(4), rebound.get(7)],
    }
}

/// Native UUID carriers agree with the server's canonical text and exact wire.
#[cfg(feature = "with-uuid-1")]
#[compio::test]
async fn native_uuid_codecs_match_server_text_and_wire() {
    let theirs = tokio_native_uuid_observation(common::plaintext_url());
    let ours = compio_native_uuid_observation().await;
    assert_eq!(ours, theirs);

    let expected_text = [
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "f81d4fae-7dec-4a0c-a765-00a0c91e6bf6",
    ];
    let expected_values = expected_text.map(|text| uuid::Uuid::parse_str(text).unwrap());
    let expected_wires = [
        "00000000000000000000000000000000",
        "ffffffffffffffffffffffffffffffff",
        "f81d4fae7dec4a0ca76500a0c91e6bf6",
    ];

    assert_eq!(ours.decoded, expected_values);
    assert_eq!(ours.rebound, expected_values);
    assert_eq!(ours.server_text, expected_text);
    assert_eq!(ours.rebound_text, expected_text);
    for (index, expected_wire) in expected_wires.into_iter().enumerate() {
        assert_eq!(hex(&ours.server_wires[index].0), expected_wire);
        assert_eq!(hex(&ours.outbound_wires[index]), expected_wire);
        assert_eq!(hex(&ours.rebound_wires[index].0), expected_wire);
    }
}

#[cfg(feature = "with-serde_json-1")]
const NATIVE_JSON_SOURCE_TEXT: &str = r#"{"zz":0,"a":1,"bbb":2,"aa":3}"#;

#[cfg(feature = "with-serde_json-1")]
const NATIVE_SERDE_JSON_TEXT: &str = r#"{"a":1,"aa":3,"bbb":2,"zz":0}"#;

#[cfg(feature = "with-serde_json-1")]
const NATIVE_SERVER_JSONB_TEXT: &str = r#"{"a": 1, "aa": 3, "zz": 0, "bbb": 2}"#;

#[cfg(feature = "with-serde_json-1")]
const NATIVE_JSON_DECODE_SQL: &str = r#"WITH fixture(value) AS (
    VALUES ($json${"zz":0,"a":1,"bbb":2,"aa":3}$json$::text)
)
SELECT
    value::json, value::json, (value::json)::text,
    value::jsonb, value::jsonb, (value::jsonb)::text
FROM fixture"#;

#[cfg(feature = "with-serde_json-1")]
const NATIVE_JSON_REBOUND_SQL: &str =
    "SELECT $1::json, ($1::json)::text, $2::jsonb, ($2::jsonb)::text";

#[cfg(feature = "with-serde_json-1")]
#[derive(Debug, PartialEq, Eq)]
struct NativeJsonObservation {
    decoded: [serde_json::Value; 2],
    decoded_serialized: [String; 2],
    server_text: [String; 2],
    server_wires: [Wire; 2],
    outbound_wires: [Vec<u8>; 2],
    rebound_text: [String; 2],
    rebound_wires: [Wire; 2],
    version_two_error: String,
}

#[cfg(feature = "with-serde_json-1")]
fn expected_jsonb_wire(text: &str) -> Vec<u8> {
    let mut wire = Vec::with_capacity(text.len() + 1);
    wire.push(1);
    wire.extend_from_slice(text.as_bytes());
    wire
}

#[cfg(feature = "with-serde_json-1")]
fn tokio_json_wire(value: &serde_json::Value, ty: &tokio_types::Type) -> Vec<u8> {
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("tokio-postgres native JSON encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-serde_json-1")]
fn compio_json_wire(value: &serde_json::Value, ty: &compio_types::Type) -> Vec<u8> {
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("compio-postgres native JSON encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-serde_json-1")]
fn tokio_jsonb_version_two_error() -> String {
    <serde_json::Value as tokio_types::FromSql>::from_sql(
        &tokio_types::Type::JSONB,
        &[2, b'{', b'}'],
    )
    .expect_err("tokio-postgres accepted JSONB version 2")
    .to_string()
}

#[cfg(feature = "with-serde_json-1")]
fn compio_jsonb_version_two_error() -> String {
    <serde_json::Value as compio_types::FromSql>::from_sql(
        &compio_types::Type::JSONB,
        &[2, b'{', b'}'],
    )
    .expect_err("compio-postgres accepted JSONB version 2")
    .to_string()
}

#[cfg(feature = "with-serde_json-1")]
fn tokio_native_json_observation(url: String) -> NativeJsonObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_JSON_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native JSON decode");
        let decoded = [row.get(0), row.get(3)];
        let decoded_serialized = decoded.each_ref().map(|value| {
            serde_json::to_string(value).expect("serialize tokio-postgres native JSON")
        });
        let outbound_wires = [
            tokio_json_wire(&decoded[0], &tokio_types::Type::JSON),
            tokio_json_wire(&decoded[1], &tokio_types::Type::JSONB),
        ];
        let rebound = client
            .query_one(NATIVE_JSON_REBOUND_SQL, &[&decoded[0], &decoded[1]])
            .await
            .expect("tokio-postgres native JSON encode");

        NativeJsonObservation {
            decoded,
            decoded_serialized,
            server_text: [row.get(2), row.get(5)],
            server_wires: [row.get(1), row.get(4)],
            outbound_wires,
            rebound_text: [rebound.get(1), rebound.get(3)],
            rebound_wires: [rebound.get(0), rebound.get(2)],
            version_two_error: tokio_jsonb_version_two_error(),
        }
    })
}

#[cfg(feature = "with-serde_json-1")]
#[allow(clippy::future_not_send)]
async fn compio_native_json_observation() -> NativeJsonObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_JSON_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native JSON decode");
    let decoded = [row.get(0), row.get(3)];
    let decoded_serialized = decoded
        .each_ref()
        .map(|value| serde_json::to_string(value).expect("serialize compio-postgres native JSON"));
    let outbound_wires = [
        compio_json_wire(&decoded[0], &compio_types::Type::JSON),
        compio_json_wire(&decoded[1], &compio_types::Type::JSONB),
    ];
    let rebound = client
        .query_one(NATIVE_JSON_REBOUND_SQL, &[&decoded[0], &decoded[1]])
        .await
        .expect("compio-postgres native JSON encode");

    NativeJsonObservation {
        decoded,
        decoded_serialized,
        server_text: [row.get(2), row.get(5)],
        server_wires: [row.get(1), row.get(4)],
        outbound_wires,
        rebound_text: [rebound.get(1), rebound.get(3)],
        rebound_wires: [rebound.get(0), rebound.get(2)],
        version_two_error: compio_jsonb_version_two_error(),
    }
}

/// Native serde conversion exposes three distinct object-key orders: source
/// JSON, `serde_json` serialization, and server-canonical JSONB.
#[cfg(feature = "with-serde_json-1")]
#[compio::test]
async fn native_serde_json_codecs_cover_json_and_jsonb_wire_contracts() {
    let theirs = tokio_native_json_observation(common::plaintext_url());
    let ours = compio_native_json_observation().await;
    assert_eq!(ours, theirs);

    assert_eq!(ours.decoded[0], ours.decoded[1]);
    assert_ne!(NATIVE_JSON_SOURCE_TEXT, NATIVE_SERDE_JSON_TEXT);
    assert_ne!(NATIVE_SERDE_JSON_TEXT, NATIVE_SERVER_JSONB_TEXT);
    assert_eq!(
        ours.decoded_serialized,
        [NATIVE_SERDE_JSON_TEXT, NATIVE_SERDE_JSON_TEXT]
    );
    assert_eq!(
        ours.server_text,
        [NATIVE_JSON_SOURCE_TEXT, NATIVE_SERVER_JSONB_TEXT]
    );
    assert_eq!(
        ours.rebound_text,
        [NATIVE_SERDE_JSON_TEXT, NATIVE_SERVER_JSONB_TEXT]
    );

    assert_eq!(ours.server_wires[0].0, NATIVE_JSON_SOURCE_TEXT.as_bytes());
    assert_eq!(
        ours.server_wires[1].0,
        expected_jsonb_wire(NATIVE_SERVER_JSONB_TEXT)
    );
    assert_eq!(ours.outbound_wires[0], NATIVE_SERDE_JSON_TEXT.as_bytes());
    assert_eq!(
        ours.outbound_wires[1],
        expected_jsonb_wire(NATIVE_SERDE_JSON_TEXT)
    );
    assert_eq!(ours.rebound_wires[0].0, NATIVE_SERDE_JSON_TEXT.as_bytes());
    assert_eq!(
        ours.rebound_wires[1].0,
        expected_jsonb_wire(NATIVE_SERVER_JSONB_TEXT)
    );
    assert_eq!(ours.server_wires[1].0.first(), Some(&1));
    assert_eq!(ours.outbound_wires[1].first(), Some(&1));
    assert_eq!(ours.rebound_wires[1].0.first(), Some(&1));
    assert_eq!(ours.version_two_error, "unsupported JSONB encoding version");
}

#[cfg(feature = "with-geo-types-0_7")]
const NATIVE_GEO_DECODE_SQL: &str = "SELECT \
    '(1.5,-2.25)'::point, '(1.5,-2.25)'::point, \
        ('(1.5,-2.25)'::point)::text, \
    '((3,4),(1,2))'::box, '((3,4),(1,2))'::box, \
        ('((3,4),(1,2))'::box)::text, \
    '[(1,2),(3,4),(5,6)]'::path, \
        '[(1,2),(3,4),(5,6)]'::path, \
        ('[(1,2),(3,4),(5,6)]'::path)::text, \
    '((1,2),(3,4),(5,6))'::path, \
        '((1,2),(3,4),(5,6))'::path, \
        ('((1,2),(3,4),(5,6))'::path)::text, \
    '((1,2),(3,4),(5,6))'::polygon, \
        ('((1,2),(3,4),(5,6))'::polygon)::text";

#[cfg(feature = "with-geo-types-0_7")]
const NATIVE_GEO_REBOUND_SQL: &str = "SELECT \
    $1::point, ($1::point)::text, \
    $2::box, ($2::box)::text, \
    $3::path, ($3::path)::text, \
    $4::path, ($4::path)::text, \
    $5::polygon, ($5::polygon)::text";

#[cfg(feature = "with-geo-types-0_7")]
#[derive(Debug, PartialEq)]
struct NativeGeoObservation {
    decoded_point: geo_types::Point<f64>,
    decoded_box: geo_types::Rect<f64>,
    decoded_paths: [geo_types::LineString<f64>; 2],
    server_text: [String; 5],
    server_wires: [Wire; 5],
    outbound_wires: [Vec<u8>; 4],
    rebound_text: [String; 5],
    rebound_wires: [Wire; 5],
}

#[cfg(feature = "with-geo-types-0_7")]
fn append_geo_point(wire: &mut Vec<u8>, point: (f64, f64)) {
    wire.extend_from_slice(&point.0.to_be_bytes());
    wire.extend_from_slice(&point.1.to_be_bytes());
}

#[cfg(feature = "with-geo-types-0_7")]
fn expected_geo_point_wire(point: (f64, f64)) -> Vec<u8> {
    let mut wire = Vec::with_capacity(16);
    append_geo_point(&mut wire, point);
    wire
}

#[cfg(feature = "with-geo-types-0_7")]
fn expected_geo_box_wire(first: (f64, f64), second: (f64, f64)) -> Vec<u8> {
    let mut wire = Vec::with_capacity(32);
    append_geo_point(&mut wire, first);
    append_geo_point(&mut wire, second);
    wire
}

#[cfg(feature = "with-geo-types-0_7")]
fn expected_geo_path_wire(closed: bool, points: &[(f64, f64)]) -> Vec<u8> {
    let count = i32::try_from(points.len()).expect("PATH fixture count fits i32");
    let mut wire = Vec::with_capacity(5 + points.len() * 16);
    wire.push(u8::from(closed));
    wire.extend_from_slice(&count.to_be_bytes());
    for point in points {
        append_geo_point(&mut wire, *point);
    }
    wire
}

#[cfg(feature = "with-geo-types-0_7")]
fn expected_geo_polygon_wire(points: &[(f64, f64)]) -> Vec<u8> {
    let count = i32::try_from(points.len()).expect("POLYGON fixture count fits i32");
    let mut wire = Vec::with_capacity(4 + points.len() * 16);
    wire.extend_from_slice(&count.to_be_bytes());
    for point in points {
        append_geo_point(&mut wire, *point);
    }
    wire
}

#[cfg(feature = "with-geo-types-0_7")]
fn tokio_geo_wire<T>(value: &T, ty: &tokio_types::Type) -> Vec<u8>
where
    T: tokio_types::ToSql,
{
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("tokio-postgres native geometry encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-geo-types-0_7")]
fn compio_geo_wire<T>(value: &T, ty: &compio_types::Type) -> Vec<u8>
where
    T: compio_types::ToSql,
{
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("compio-postgres native geometry encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-geo-types-0_7")]
fn tokio_native_geo_observation(url: String) -> NativeGeoObservation {
    on_tokio(url, |client| async move {
        let row = client
            .query_one(NATIVE_GEO_DECODE_SQL, &[])
            .await
            .expect("tokio-postgres native geometry decode");
        let point: geo_types::Point<f64> = row.get(0);
        let rectangle: geo_types::Rect<f64> = row.get(3);
        let open_path: geo_types::LineString<f64> = row.get(6);
        let closed_path: geo_types::LineString<f64> = row.get(9);
        let polygon_wire: Wire = row.get(12);
        let outbound_wires = [
            tokio_geo_wire(&point, &tokio_types::Type::POINT),
            tokio_geo_wire(&rectangle, &tokio_types::Type::BOX),
            tokio_geo_wire(&open_path, &tokio_types::Type::PATH),
            tokio_geo_wire(&closed_path, &tokio_types::Type::PATH),
        ];
        let rebound = client
            .query_one(
                NATIVE_GEO_REBOUND_SQL,
                &[&point, &rectangle, &open_path, &closed_path, &polygon_wire],
            )
            .await
            .expect("tokio-postgres native geometry encode");

        NativeGeoObservation {
            decoded_point: point,
            decoded_box: rectangle,
            decoded_paths: [open_path, closed_path],
            server_text: [row.get(2), row.get(5), row.get(8), row.get(11), row.get(13)],
            server_wires: [
                row.get(1),
                row.get(4),
                row.get(7),
                row.get(10),
                polygon_wire,
            ],
            outbound_wires,
            rebound_text: [
                rebound.get(1),
                rebound.get(3),
                rebound.get(5),
                rebound.get(7),
                rebound.get(9),
            ],
            rebound_wires: [
                rebound.get(0),
                rebound.get(2),
                rebound.get(4),
                rebound.get(6),
                rebound.get(8),
            ],
        }
    })
}

#[cfg(feature = "with-geo-types-0_7")]
#[allow(clippy::future_not_send)]
async fn compio_native_geo_observation() -> NativeGeoObservation {
    let client = compio_client().await;
    let row = client
        .query_one(NATIVE_GEO_DECODE_SQL, &[])
        .await
        .expect("compio-postgres native geometry decode");
    let point: geo_types::Point<f64> = row.get(0);
    let rectangle: geo_types::Rect<f64> = row.get(3);
    let open_path: geo_types::LineString<f64> = row.get(6);
    let closed_path: geo_types::LineString<f64> = row.get(9);
    let polygon_wire: Wire = row.get(12);
    let outbound_wires = [
        compio_geo_wire(&point, &compio_types::Type::POINT),
        compio_geo_wire(&rectangle, &compio_types::Type::BOX),
        compio_geo_wire(&open_path, &compio_types::Type::PATH),
        compio_geo_wire(&closed_path, &compio_types::Type::PATH),
    ];
    let rebound = client
        .query_one(
            NATIVE_GEO_REBOUND_SQL,
            &[&point, &rectangle, &open_path, &closed_path, &polygon_wire],
        )
        .await
        .expect("compio-postgres native geometry encode");

    NativeGeoObservation {
        decoded_point: point,
        decoded_box: rectangle,
        decoded_paths: [open_path, closed_path],
        server_text: [row.get(2), row.get(5), row.get(8), row.get(11), row.get(13)],
        server_wires: [
            row.get(1),
            row.get(4),
            row.get(7),
            row.get(10),
            polygon_wire,
        ],
        outbound_wires,
        rebound_text: [
            rebound.get(1),
            rebound.get(3),
            rebound.get(5),
            rebound.get(7),
            rebound.get(9),
        ],
        rebound_wires: [
            rebound.get(0),
            rebound.get(2),
            rebound.get(4),
            rebound.get(6),
            rebound.get(8),
        ],
    }
}

/// Native geometry carriers agree on coordinates. Direct bytes expose the
/// shared BOX corner-order and closed PATH state losses; POLYGON stays raw
/// because neither `geo-types` integration supplies a native polygon codec.
#[cfg(feature = "with-geo-types-0_7")]
#[compio::test]
async fn native_geo_types_codecs_cover_geometry_wires_and_shared_limits() {
    let theirs = tokio_native_geo_observation(common::plaintext_url());
    let ours = compio_native_geo_observation().await;
    assert_eq!(ours, theirs);

    let points = [(1.0, 2.0), (3.0, 4.0), (5.0, 6.0)];
    let expected_line = geo_types::LineString::from(points.to_vec());
    assert_eq!(ours.decoded_point, geo_types::Point::new(1.5, -2.25));
    assert_eq!(
        ours.decoded_box,
        geo_types::Rect::new((1.0, 2.0), (3.0, 4.0))
    );
    assert_eq!(ours.decoded_paths, [expected_line.clone(), expected_line]);
    assert_eq!(
        ours.server_text,
        [
            "(1.5,-2.25)",
            "(3,4),(1,2)",
            "[(1,2),(3,4),(5,6)]",
            "((1,2),(3,4),(5,6))",
            "((1,2),(3,4),(5,6))",
        ]
    );
    assert_eq!(
        ours.rebound_text,
        [
            "(1.5,-2.25)",
            "(3,4),(1,2)",
            "[(1,2),(3,4),(5,6)]",
            "[(1,2),(3,4),(5,6)]",
            "((1,2),(3,4),(5,6))",
        ]
    );

    let point_wire = expected_geo_point_wire((1.5, -2.25));
    let server_box_wire = expected_geo_box_wire((3.0, 4.0), (1.0, 2.0));
    let codec_box_wire = expected_geo_box_wire((1.0, 2.0), (3.0, 4.0));
    let open_path_wire = expected_geo_path_wire(false, &points);
    let closed_path_wire = expected_geo_path_wire(true, &points);
    let polygon_wire = expected_geo_polygon_wire(&points);

    assert_eq!(
        ours.server_wires.each_ref().map(|wire| wire.0.as_slice()),
        [
            point_wire.as_slice(),
            server_box_wire.as_slice(),
            open_path_wire.as_slice(),
            closed_path_wire.as_slice(),
            polygon_wire.as_slice(),
        ]
    );
    assert_eq!(
        ours.outbound_wires.each_ref().map(Vec::as_slice),
        [
            point_wire.as_slice(),
            codec_box_wire.as_slice(),
            open_path_wire.as_slice(),
            open_path_wire.as_slice(),
        ]
    );
    assert_eq!(
        ours.rebound_wires.each_ref().map(|wire| wire.0.as_slice()),
        [
            point_wire.as_slice(),
            server_box_wire.as_slice(),
            open_path_wire.as_slice(),
            open_path_wire.as_slice(),
            polygon_wire.as_slice(),
        ]
    );

    assert_ne!(ours.outbound_wires[1], ours.server_wires[1].0);
    assert_ne!(ours.outbound_wires[3], ours.server_wires[3].0);
    assert_eq!(ours.outbound_wires[2], ours.outbound_wires[3]);
}

/// Desired invariant blocked by both `Rect` codecs emitting BOX corners in
/// the reverse of the server's binary order.
/// Severity, measured against the server rather than inferred: NOT corruption.
/// `box '(1,2),(3,4)'` is sent by PostgreSQL as high corner first, 3,4,1,2, and
/// even its text form normalises to `(3,4),(1,2)`. We send 1,2,3,4. Feeding
/// BOTH orders back through `COPY ... FROM STDIN (FORMAT binary)` stores
/// `(3,4),(1,2)` either way and both compare `=` to the original, because
/// `box_recv` normalises the corners on receipt. So this is wire
/// nonconformance only - the same class as the CIDR `is_cidr` flag - and a
/// claim that it loses or swaps a value would be wrong.
#[cfg(feature = "with-geo-types-0_7")]
#[ignore = "both geo-types Rect codecs emit PostgreSQL BOX corners in reverse order"]
#[compio::test]
async fn native_geo_rect_codecs_must_emit_server_box_order() {
    let theirs = tokio_native_geo_observation(common::plaintext_url());
    let ours = compio_native_geo_observation().await;
    assert_eq!(ours.outbound_wires[1], ours.server_wires[1].0);
    assert_eq!(theirs.outbound_wires[1], theirs.server_wires[1].0);
}

/// Desired invariant blocked because `LineString` has no closed-path state.
#[cfg(feature = "with-geo-types-0_7")]
#[ignore = "both geo-types LineString codecs discard PostgreSQL PATH closed state"]
#[compio::test]
async fn native_geo_path_codecs_must_preserve_closed_flag() {
    let theirs = tokio_native_geo_observation(common::plaintext_url());
    let ours = compio_native_geo_observation().await;
    assert_eq!(ours.outbound_wires[3], ours.server_wires[3].0);
    assert_eq!(theirs.outbound_wires[3], theirs.server_wires[3].0);
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

fn numeric_format_cases() -> Vec<RawCase> {
    vec![
        RawCase::new(
            "numeric-zero-scale-40",
            "'0.0000000000000000000000000000000000000000'::numeric",
            "numeric",
        ),
        RawCase::new("numeric-trailing-zeros", "'123.450000'::numeric", "numeric"),
        RawCase::new(
            "numeric-large-positive-weight",
            "'1e1000'::numeric",
            "numeric",
        ),
        RawCase::new(
            "numeric-large-negative-weight",
            "'1e-1000'::numeric",
            "numeric",
        ),
        RawCase::new(
            "numeric-beyond-float-precision",
            "'1234567890123456789012345678901234567890.123456789012345678901234567890'::numeric",
            "numeric",
        ),
        RawCase::new("numeric-nan-format", "'NaN'::numeric", "numeric"),
        RawCase::new(
            "numeric-positive-infinity-format",
            "'Infinity'::numeric",
            "numeric",
        ),
        RawCase::new(
            "numeric-negative-infinity-format",
            "'-Infinity'::numeric",
            "numeric",
        ),
    ]
}

fn tokio_numeric_format_observations(url: String, cases: Vec<RawCase>) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_numeric_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

/// Both drivers preserve NUMERIC's scale, weight, signs, and exact digits.
#[compio::test]
async fn both_drivers_agree_on_numeric_text_and_binary_codecs() {
    let cases = numeric_format_cases();
    let theirs = tokio_numeric_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_numeric_format_observations(&cases).await;
    assert_format_differential(&cases, &ours, &theirs);

    let positive_weight_text = format!("1{}", "0".repeat(1000));
    let negative_weight_text = format!("0.{}1", "0".repeat(999));
    let expected = [
        (
            "numeric-zero-scale-40",
            "0.0000000000000000000000000000000000000000",
            "0000000000000028",
        ),
        (
            "numeric-trailing-zeros",
            "123.450000",
            "0002000000000006007b1194",
        ),
        (
            "numeric-large-positive-weight",
            positive_weight_text.as_str(),
            "000100fa000000000001",
        ),
        (
            "numeric-large-negative-weight",
            negative_weight_text.as_str(),
            "0001ff06000003e80001",
        ),
        ("numeric-nan-format", "NaN", "00000000c0000000"),
        (
            "numeric-positive-infinity-format",
            "Infinity",
            "00000000d0000020",
        ),
        (
            "numeric-negative-infinity-format",
            "-Infinity",
            "00000000f0000020",
        ),
    ];
    for (name, text, binary_hex) in expected {
        let observation = ours
            .iter()
            .find(|observation| observation.name == name)
            .unwrap_or_else(|| panic!("no numeric observation named {name}"));
        assert_eq!(observation.text_decoded, text, "{name}: server text");
        assert_eq!(
            hex(&observation.binary_decoded.0),
            binary_hex,
            "{name}: server binary"
        );
    }

    let precise = ours
        .iter()
        .find(|observation| observation.name == "numeric-beyond-float-precision")
        .expect("numeric precision observation");
    assert_eq!(
        precise.text_decoded,
        "1234567890123456789012345678901234567890.123456789012345678901234567890"
    );
    let precise_wire =
        decode_numeric(&precise.binary_decoded.0).expect("decode precise NUMERIC wire value");
    assert_eq!(precise_wire.weight, 9);
    assert_eq!(precise_wire.display_scale, 30);
    assert_eq!(precise_wire.digits.len(), 18);
    assert!(precise_wire.digits.iter().all(|digit| *digit < 10_000));
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

fn temporal_format_cases() -> Vec<RawCase> {
    vec![
        RawCase::new("temporal-date-bc", "'4714-11-24 BC'::date", "date"),
        RawCase::new(
            "temporal-date-negative-infinity",
            "'-infinity'::date",
            "date",
        ),
        RawCase::new(
            "temporal-date-positive-infinity",
            "'infinity'::date",
            "date",
        ),
        RawCase::new(
            "temporal-timestamp-bc",
            "'4714-11-24 00:00:00 BC'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "temporal-timestamp-microsecond",
            "'1999-12-31 23:59:59.999999'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "temporal-timestamp-negative-infinity",
            "'-infinity'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "temporal-timestamp-positive-infinity",
            "'infinity'::timestamp",
            "timestamp",
        ),
        RawCase::new(
            "temporal-timestamptz-offset-microsecond",
            "'2001-02-03 04:05:06.123456+05:45'::timestamptz",
            "timestamptz",
        ),
        RawCase::new(
            "temporal-timestamptz-negative-infinity",
            "'-infinity'::timestamptz",
            "timestamptz",
        ),
        RawCase::new(
            "temporal-timestamptz-positive-infinity",
            "'infinity'::timestamptz",
            "timestamptz",
        ),
        RawCase::new(
            "temporal-time-last-microsecond",
            "'23:59:59.999999'::time",
            "time",
        ),
        RawCase::new("temporal-time-end-of-day", "'24:00'::time", "time"),
        RawCase::new(
            "temporal-interval-mixed-signs",
            "'1 mon -2 days 03:04:05.000006'::interval",
            "interval",
        ),
    ]
}

fn tokio_temporal_format_observations(url: String, cases: Vec<RawCase>) -> Vec<FormatObservation> {
    tokio_format_observations(url, cases)
}

#[allow(clippy::future_not_send)]
async fn compio_temporal_format_observations(cases: &[RawCase]) -> Vec<FormatObservation> {
    compio_format_observations(cases).await
}

/// Whether the server takes `interval 'infinity'`, and what it answers.
///
/// This is a server-version fork, not a driver difference: the server gained
/// interval infinity after 16, so the same query is a syntax error on one
/// supported server and a value on another. Recording the OUTCOME rather than
/// asserting a refusal keeps the differential claim - both drivers say the
/// same thing - independent of which server is answering.
fn tokio_interval_infinity_states(url: String) -> Vec<String> {
    on_tokio(url, |client| async move {
        let mut states = Vec::new();
        for expression in ["'infinity'::interval", "'-infinity'::interval"] {
            match client
                .query_one(&format!("SELECT ({expression})::text"), &[])
                .await
            {
                Ok(row) => states.push(format!("ok:{}", row.get::<_, String>(0))),
                Err(error) => states.push(format!(
                    "err:{}",
                    error
                        .code()
                        .expect("interval infinity refusal had no SQLSTATE")
                        .code()
                )),
            }
        }
        states
    })
}

#[allow(clippy::future_not_send)]
async fn compio_interval_infinity_states() -> Vec<String> {
    let client = compio_client().await;
    let mut states = Vec::new();
    for expression in ["'infinity'::interval", "'-infinity'::interval"] {
        match client
            .query_one(&format!("SELECT ({expression})::text"), &[])
            .await
        {
            Ok(row) => states.push(format!("ok:{}", row.get::<_, String>(0))),
            Err(error) => states.push(format!(
                "err:{}",
                error
                    .code()
                    .expect("interval infinity refusal had no SQLSTATE")
                    .code()
            )),
        }
    }
    states
}

/// Both drivers preserve temporal values through text and binary formats.
#[compio::test]
async fn both_drivers_agree_on_temporal_text_and_binary_codecs() {
    let cases = temporal_format_cases();
    let theirs = tokio_temporal_format_observations(common::plaintext_url(), cases.clone());
    let ours = compio_temporal_format_observations(&cases).await;
    assert_format_differential(&cases, &ours, &theirs);

    let expected = [
        ("temporal-date-bc", "4714-11-24 BC", "ffda97a7"),
        ("temporal-date-negative-infinity", "-infinity", "80000000"),
        ("temporal-date-positive-infinity", "infinity", "7fffffff"),
        (
            "temporal-timestamp-bc",
            "4714-11-24 00:00:00 BC",
            "fd0f7cc1411fa000",
        ),
        (
            "temporal-timestamp-microsecond",
            "1999-12-31 23:59:59.999999",
            "ffffffffffffffff",
        ),
        (
            "temporal-timestamp-negative-infinity",
            "-infinity",
            "8000000000000000",
        ),
        (
            "temporal-timestamp-positive-infinity",
            "infinity",
            "7fffffffffffffff",
        ),
        (
            "temporal-timestamptz-negative-infinity",
            "-infinity",
            "8000000000000000",
        ),
        (
            "temporal-timestamptz-positive-infinity",
            "infinity",
            "7fffffffffffffff",
        ),
        (
            "temporal-time-last-microsecond",
            "23:59:59.999999",
            "000000141dd75fff",
        ),
        ("temporal-time-end-of-day", "24:00:00", "000000141dd76000"),
    ];
    for (name, text, binary_hex) in expected {
        let observation = ours
            .iter()
            .find(|observation| observation.name == name)
            .unwrap_or_else(|| panic!("no temporal observation named {name}"));
        assert_eq!(observation.text_decoded, text, "{name}: server text");
        assert_eq!(
            hex(&observation.binary_decoded.0),
            binary_hex,
            "{name}: server binary"
        );
    }

    let zoned = ours
        .iter()
        .find(|observation| observation.name == "temporal-timestamptz-offset-microsecond")
        .expect("offset timestamptz observation");
    assert_eq!(zoned.text_decoded, "2001-02-02 22:20:06.123456+00");
    assert_eq!(
        i64::from_be_bytes(zoned.binary_decoded.0.as_slice().try_into().unwrap()),
        34_467_606_123_456
    );

    let interval = ours
        .iter()
        .find(|observation| observation.name == "temporal-interval-mixed-signs")
        .expect("interval observation");
    assert_eq!(
        decode_interval(&interval.binary_decoded.0).unwrap(),
        IntervalWire {
            microseconds: 11_045_000_006,
            days: -2,
            months: 1,
        }
    );

    let theirs = tokio_interval_infinity_states(common::plaintext_url());
    let ours = compio_interval_infinity_states().await;
    assert_eq!(
        ours, theirs,
        "the drivers disagreed about interval infinity"
    );

    // PostgreSQL 17 introduced interval infinity. Below that the literal is a
    // syntax error; at or above it, it is a value. Measured on both servers
    // this suite runs against: 160014 refuses, 180004 renders "infinity".
    let server: i32 = compio_client()
        .await
        .query_one_scalar("SELECT current_setting('server_version_num')::int4", &[])
        .await
        .expect("read server_version_num");
    let expected: [String; 2] = if server < 170_000 {
        ["err:22007".to_owned(), "err:22007".to_owned()]
    } else {
        ["ok:infinity".to_owned(), "ok:-infinity".to_owned()]
    };
    assert_eq!(ours, expected, "server_version_num={server}");
}

const TYPED_TEMPORAL_SQL: &str = "SELECT \
     '0001-01-01 BC'::date, \
     '1999-12-31 23:59:59.999999'::timestamp, \
     '2001-02-03 04:05:06.123456+05:45'::timestamptz, \
     '23:59:59.999999'::time, \
     '-infinity'::date, 'infinity'::date, \
     '-infinity'::timestamp, 'infinity'::timestamp, \
     '-infinity'::timestamptz, 'infinity'::timestamptz, \
     '24:00'::time";

const TYPED_TEMPORAL_REBOUND_SQL: &str = "SELECT \
     $1::date::text, $2::timestamp::text, $3::timestamptz::text, $4::time::text, \
     $5::date::text, $6::date::text, \
     $7::timestamp::text, $8::timestamp::text, \
     $9::timestamptz::text, $10::timestamptz::text";

#[cfg(feature = "with-jiff-0_2")]
#[derive(Debug, PartialEq, Eq)]
struct BoundJiffTimeObservation {
    server_text: String,
    server_wire: Wire,
    decoded: ValueOutcome<String>,
}

#[cfg(feature = "with-jiff-0_2")]
#[derive(Debug, PartialEq, Eq)]
struct NativeJiffObservation {
    decoded: [String; 4],
    server_wires: [Wire; 11],
    outbound_wires: [Vec<u8>; 10],
    rebound: Vec<String>,
    time_24: ValueOutcome<String>,
    submicro_input: String,
    submicro_outbound_wire: Vec<u8>,
    submicro_bind: ValueOutcome<BoundJiffTimeObservation>,
}

#[cfg(feature = "with-jiff-0_2")]
fn tokio_jiff_wire<T>(value: &T, ty: &tokio_types::Type) -> Vec<u8>
where
    T: tokio_types::ToSql,
{
    let mut wire = tokio_types::private::BytesMut::new();
    let is_null = tokio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("tokio-postgres native Jiff encode");
    assert!(matches!(is_null, tokio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-jiff-0_2")]
fn compio_jiff_wire<T>(value: &T, ty: &compio_types::Type) -> Vec<u8>
where
    T: compio_types::ToSql,
{
    let mut wire = compio_types::private::BytesMut::new();
    let is_null = compio_types::ToSql::to_sql_checked(value, ty, &mut wire)
        .expect("compio-postgres native Jiff encode");
    assert!(matches!(is_null, compio_types::IsNull::No));
    wire.to_vec()
}

#[cfg(feature = "with-jiff-0_2")]
fn tokio_native_jiff_observation(url: String) -> NativeJiffObservation {
    on_tokio(url, |client| async move {
        client
            .batch_execute(FORMAT_RENDERING_SQL)
            .await
            .expect("set tokio Jiff temporal rendering");
        let row = client
            .query_one(TYPED_TEMPORAL_SQL, &[])
            .await
            .expect("tokio-postgres native Jiff decode");

        let date: jiff::civil::Date = row.get(0);
        let datetime: jiff::civil::DateTime = row.get(1);
        let timestamp: jiff::Timestamp = row.get(2);
        let time: jiff::civil::Time = row.get(3);
        let date_neg: tokio_types::Date<jiff::civil::Date> = row.get(4);
        let date_pos: tokio_types::Date<jiff::civil::Date> = row.get(5);
        let datetime_neg: tokio_types::Timestamp<jiff::civil::DateTime> = row.get(6);
        let datetime_pos: tokio_types::Timestamp<jiff::civil::DateTime> = row.get(7);
        let timestamp_neg: tokio_types::Timestamp<jiff::Timestamp> = row.get(8);
        let timestamp_pos: tokio_types::Timestamp<jiff::Timestamp> = row.get(9);
        let time_24 = match row.try_get::<_, jiff::civil::Time>(10) {
            Ok(value) => ValueOutcome::Value(value.to_string()),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };

        let server_wires = std::array::from_fn(|index| row.get(index));
        let outbound_wires = [
            tokio_jiff_wire(&date, &tokio_types::Type::DATE),
            tokio_jiff_wire(&datetime, &tokio_types::Type::TIMESTAMP),
            tokio_jiff_wire(&timestamp, &tokio_types::Type::TIMESTAMPTZ),
            tokio_jiff_wire(&time, &tokio_types::Type::TIME),
            tokio_jiff_wire(&date_neg, &tokio_types::Type::DATE),
            tokio_jiff_wire(&date_pos, &tokio_types::Type::DATE),
            tokio_jiff_wire(&datetime_neg, &tokio_types::Type::TIMESTAMP),
            tokio_jiff_wire(&datetime_pos, &tokio_types::Type::TIMESTAMP),
            tokio_jiff_wire(&timestamp_neg, &tokio_types::Type::TIMESTAMPTZ),
            tokio_jiff_wire(&timestamp_pos, &tokio_types::Type::TIMESTAMPTZ),
        ];
        let rebound = client
            .query_one(
                TYPED_TEMPORAL_REBOUND_SQL,
                &[
                    &date,
                    &datetime,
                    &timestamp,
                    &time,
                    &date_neg,
                    &date_pos,
                    &datetime_neg,
                    &datetime_pos,
                    &timestamp_neg,
                    &timestamp_pos,
                ],
            )
            .await
            .expect("tokio-postgres native Jiff rebound");

        let submicro = jiff::civil::Time::new(23, 59, 59, 999_999_500)
            .expect("construct Jiff sub-microsecond edge");
        let submicro_outbound_wire = tokio_jiff_wire(&submicro, &tokio_types::Type::TIME);
        let submicro_bind = match client
            .query_one("SELECT ($1::time)::text, $1::time", &[&submicro])
            .await
        {
            Ok(row) => {
                let decoded = match row.try_get::<_, jiff::civil::Time>(1) {
                    Ok(value) => ValueOutcome::Value(value.to_string()),
                    Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
                    Err(error) => {
                        ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned())
                    }
                };
                ValueOutcome::Value(BoundJiffTimeObservation {
                    server_text: row.get(0),
                    server_wire: row.get(1),
                    decoded,
                })
            }
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };

        NativeJiffObservation {
            decoded: [
                date.to_string(),
                datetime.to_string(),
                timestamp.to_string(),
                time.to_string(),
            ],
            server_wires,
            outbound_wires,
            rebound: (0..10).map(|index| rebound.get(index)).collect(),
            time_24,
            submicro_input: submicro.to_string(),
            submicro_outbound_wire,
            submicro_bind,
        }
    })
}

#[cfg(feature = "with-jiff-0_2")]
#[allow(clippy::future_not_send)]
async fn compio_native_jiff_observation() -> NativeJiffObservation {
    let client = compio_client().await;
    client
        .batch_execute(FORMAT_RENDERING_SQL)
        .await
        .expect("set compio Jiff temporal rendering");
    let row = client
        .query_one(TYPED_TEMPORAL_SQL, &[])
        .await
        .expect("compio-postgres native Jiff decode");

    let date: jiff::civil::Date = row.get(0);
    let datetime: jiff::civil::DateTime = row.get(1);
    let timestamp: jiff::Timestamp = row.get(2);
    let time: jiff::civil::Time = row.get(3);
    let date_neg: compio_types::Date<jiff::civil::Date> = row.get(4);
    let date_pos: compio_types::Date<jiff::civil::Date> = row.get(5);
    let datetime_neg: compio_types::Timestamp<jiff::civil::DateTime> = row.get(6);
    let datetime_pos: compio_types::Timestamp<jiff::civil::DateTime> = row.get(7);
    let timestamp_neg: compio_types::Timestamp<jiff::Timestamp> = row.get(8);
    let timestamp_pos: compio_types::Timestamp<jiff::Timestamp> = row.get(9);
    let time_24 = match row.try_get::<_, jiff::civil::Time>(10) {
        Ok(value) => ValueOutcome::Value(value.to_string()),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };

    let server_wires = std::array::from_fn(|index| row.get(index));
    let outbound_wires = [
        compio_jiff_wire(&date, &compio_types::Type::DATE),
        compio_jiff_wire(&datetime, &compio_types::Type::TIMESTAMP),
        compio_jiff_wire(&timestamp, &compio_types::Type::TIMESTAMPTZ),
        compio_jiff_wire(&time, &compio_types::Type::TIME),
        compio_jiff_wire(&date_neg, &compio_types::Type::DATE),
        compio_jiff_wire(&date_pos, &compio_types::Type::DATE),
        compio_jiff_wire(&datetime_neg, &compio_types::Type::TIMESTAMP),
        compio_jiff_wire(&datetime_pos, &compio_types::Type::TIMESTAMP),
        compio_jiff_wire(&timestamp_neg, &compio_types::Type::TIMESTAMPTZ),
        compio_jiff_wire(&timestamp_pos, &compio_types::Type::TIMESTAMPTZ),
    ];
    let rebound = client
        .query_one(
            TYPED_TEMPORAL_REBOUND_SQL,
            &[
                &date,
                &datetime,
                &timestamp,
                &time,
                &date_neg,
                &date_pos,
                &datetime_neg,
                &datetime_pos,
                &timestamp_neg,
                &timestamp_pos,
            ],
        )
        .await
        .expect("compio-postgres native Jiff rebound");

    let submicro = jiff::civil::Time::new(23, 59, 59, 999_999_500)
        .expect("construct Jiff sub-microsecond edge");
    let submicro_outbound_wire = compio_jiff_wire(&submicro, &compio_types::Type::TIME);
    let submicro_bind = match client
        .query_one("SELECT ($1::time)::text, $1::time", &[&submicro])
        .await
    {
        Ok(row) => {
            let decoded = match row.try_get::<_, jiff::civil::Time>(1) {
                Ok(value) => ValueOutcome::Value(value.to_string()),
                Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
                Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
            };
            ValueOutcome::Value(BoundJiffTimeObservation {
                server_text: row.get(0),
                server_wire: row.get(1),
                decoded,
            })
        }
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };

    NativeJiffObservation {
        decoded: [
            date.to_string(),
            datetime.to_string(),
            timestamp.to_string(),
            time.to_string(),
        ],
        server_wires,
        outbound_wires,
        rebound: (0..10).map(|index| rebound.get(index)).collect(),
        time_24,
        submicro_input: submicro.to_string(),
        submicro_outbound_wire,
        submicro_bind,
    }
}

/// Jiff's finite carriers and generic infinity wrappers agree with the server.
/// Its bare time refuses 24:00, while sub-microsecond input truncates to the
/// last representable microsecond instead of producing an undecodable 24:00.
#[cfg(feature = "with-jiff-0_2")]
#[compio::test]
async fn native_jiff_codecs_cover_temporal_edges_and_remain_closed() {
    let theirs = tokio_native_jiff_observation(common::plaintext_url());
    let ours = compio_native_jiff_observation().await;
    assert_eq!(ours, theirs);

    assert_eq!(
        ours.decoded,
        [
            "0000-01-01",
            "1999-12-31T23:59:59.999999",
            "2001-02-02T22:20:06.123456Z",
            "23:59:59.999999",
        ]
    );
    assert_eq!(
        ours.rebound,
        [
            "0001-01-01 BC",
            "1999-12-31 23:59:59.999999",
            "2001-02-02 22:20:06.123456+00",
            "23:59:59.999999",
            "-infinity",
            "infinity",
            "-infinity",
            "infinity",
            "-infinity",
            "infinity",
        ]
    );

    let expected_wires = [
        "fff4da8b",
        "ffffffffffffffff",
        "00001f591d6b53c0",
        "000000141dd75fff",
        "80000000",
        "7fffffff",
        "8000000000000000",
        "7fffffffffffffff",
        "8000000000000000",
        "7fffffffffffffff",
        "000000141dd76000",
    ];
    for (index, expected_wire) in expected_wires.into_iter().enumerate() {
        assert_eq!(hex(&ours.server_wires[index].0), expected_wire);
        if index < ours.outbound_wires.len() {
            assert_eq!(hex(&ours.outbound_wires[index]), expected_wire);
        }
    }
    assert_eq!(ours.time_24, ValueOutcome::LocalFailure);

    assert_eq!(ours.submicro_input, "23:59:59.9999995");
    assert_eq!(hex(&ours.submicro_outbound_wire), "000000141dd75fff");
    let ValueOutcome::Value(bound) = &ours.submicro_bind else {
        panic!(
            "Jiff sub-microsecond bind did not reach the server: {:?}",
            ours.submicro_bind
        );
    };
    assert_eq!(bound.server_text, "23:59:59.999999");
    assert_eq!(hex(&bound.server_wire.0), "000000141dd75fff");
    assert_eq!(
        bound.decoded,
        ValueOutcome::Value("23:59:59.999999".to_owned())
    );
}

#[cfg(all(feature = "with-chrono-0_4", feature = "with-time-0_3"))]
#[derive(Debug, PartialEq, Eq)]
struct TypedTemporalObservation {
    chrono_rebound: Vec<String>,
    time_rebound: Vec<String>,
    chrono_time_24: ValueOutcome<String>,
    time_time_24: ValueOutcome<String>,
}

#[cfg(all(feature = "with-chrono-0_4", feature = "with-time-0_3"))]
fn tokio_typed_temporal_observation(url: String) -> TypedTemporalObservation {
    on_tokio(url, |client| async move {
        client
            .batch_execute(FORMAT_RENDERING_SQL)
            .await
            .expect("set tokio temporal rendering");
        let row = client
            .query_one(TYPED_TEMPORAL_SQL, &[])
            .await
            .expect("tokio typed temporal decode");

        let chrono_date: chrono::NaiveDate = row.get(0);
        let chrono_timestamp: chrono::NaiveDateTime = row.get(1);
        let chrono_timestamptz: chrono::DateTime<chrono::Utc> = row.get(2);
        let chrono_time: chrono::NaiveTime = row.get(3);
        let chrono_date_neg: tokio_types::Date<chrono::NaiveDate> = row.get(4);
        let chrono_date_pos: tokio_types::Date<chrono::NaiveDate> = row.get(5);
        let chrono_timestamp_neg: tokio_types::Timestamp<chrono::NaiveDateTime> = row.get(6);
        let chrono_timestamp_pos: tokio_types::Timestamp<chrono::NaiveDateTime> = row.get(7);
        let chrono_timestamptz_neg: tokio_types::Timestamp<chrono::DateTime<chrono::Utc>> =
            row.get(8);
        let chrono_timestamptz_pos: tokio_types::Timestamp<chrono::DateTime<chrono::Utc>> =
            row.get(9);
        let chrono_time_24 = match row.try_get::<_, chrono::NaiveTime>(10) {
            Ok(value) => ValueOutcome::Value(value.to_string()),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };
        let rebound = client
            .query_one(
                TYPED_TEMPORAL_REBOUND_SQL,
                &[
                    &chrono_date,
                    &chrono_timestamp,
                    &chrono_timestamptz,
                    &chrono_time,
                    &chrono_date_neg,
                    &chrono_date_pos,
                    &chrono_timestamp_neg,
                    &chrono_timestamp_pos,
                    &chrono_timestamptz_neg,
                    &chrono_timestamptz_pos,
                ],
            )
            .await
            .expect("tokio chrono temporal rebound");
        let chrono_rebound = (0..10).map(|index| rebound.get(index)).collect();

        let time_date: time::Date = row.get(0);
        let time_timestamp: time::PrimitiveDateTime = row.get(1);
        let time_timestamptz: time::OffsetDateTime = row.get(2);
        let time_time: time::Time = row.get(3);
        let time_date_neg: tokio_types::Date<time::Date> = row.get(4);
        let time_date_pos: tokio_types::Date<time::Date> = row.get(5);
        let time_timestamp_neg: tokio_types::Timestamp<time::PrimitiveDateTime> = row.get(6);
        let time_timestamp_pos: tokio_types::Timestamp<time::PrimitiveDateTime> = row.get(7);
        let time_timestamptz_neg: tokio_types::Timestamp<time::OffsetDateTime> = row.get(8);
        let time_timestamptz_pos: tokio_types::Timestamp<time::OffsetDateTime> = row.get(9);
        let time_time_24 = match row.try_get::<_, time::Time>(10) {
            Ok(value) => ValueOutcome::Value(value.to_string()),
            Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
            Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
        };
        let rebound = client
            .query_one(
                TYPED_TEMPORAL_REBOUND_SQL,
                &[
                    &time_date,
                    &time_timestamp,
                    &time_timestamptz,
                    &time_time,
                    &time_date_neg,
                    &time_date_pos,
                    &time_timestamp_neg,
                    &time_timestamp_pos,
                    &time_timestamptz_neg,
                    &time_timestamptz_pos,
                ],
            )
            .await
            .expect("tokio time temporal rebound");
        let time_rebound = (0..10).map(|index| rebound.get(index)).collect();

        TypedTemporalObservation {
            chrono_rebound,
            time_rebound,
            chrono_time_24,
            time_time_24,
        }
    })
}

#[cfg(all(feature = "with-chrono-0_4", feature = "with-time-0_3"))]
#[allow(clippy::future_not_send)]
async fn compio_typed_temporal_observation() -> TypedTemporalObservation {
    let client = compio_client().await;
    client
        .batch_execute(FORMAT_RENDERING_SQL)
        .await
        .expect("set compio temporal rendering");
    let row = client
        .query_one(TYPED_TEMPORAL_SQL, &[])
        .await
        .expect("compio typed temporal decode");

    let chrono_date: chrono::NaiveDate = row.get(0);
    let chrono_timestamp: chrono::NaiveDateTime = row.get(1);
    let chrono_timestamptz: chrono::DateTime<chrono::Utc> = row.get(2);
    let chrono_time: chrono::NaiveTime = row.get(3);
    let chrono_date_neg: compio_types::Date<chrono::NaiveDate> = row.get(4);
    let chrono_date_pos: compio_types::Date<chrono::NaiveDate> = row.get(5);
    let chrono_timestamp_neg: compio_types::Timestamp<chrono::NaiveDateTime> = row.get(6);
    let chrono_timestamp_pos: compio_types::Timestamp<chrono::NaiveDateTime> = row.get(7);
    let chrono_timestamptz_neg: compio_types::Timestamp<chrono::DateTime<chrono::Utc>> = row.get(8);
    let chrono_timestamptz_pos: compio_types::Timestamp<chrono::DateTime<chrono::Utc>> = row.get(9);
    let chrono_time_24 = match row.try_get::<_, chrono::NaiveTime>(10) {
        Ok(value) => ValueOutcome::Value(value.to_string()),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };
    let rebound = client
        .query_one(
            TYPED_TEMPORAL_REBOUND_SQL,
            &[
                &chrono_date,
                &chrono_timestamp,
                &chrono_timestamptz,
                &chrono_time,
                &chrono_date_neg,
                &chrono_date_pos,
                &chrono_timestamp_neg,
                &chrono_timestamp_pos,
                &chrono_timestamptz_neg,
                &chrono_timestamptz_pos,
            ],
        )
        .await
        .expect("compio chrono temporal rebound");
    let chrono_rebound = (0..10).map(|index| rebound.get(index)).collect();

    let time_date: time::Date = row.get(0);
    let time_timestamp: time::PrimitiveDateTime = row.get(1);
    let time_timestamptz: time::OffsetDateTime = row.get(2);
    let time_time: time::Time = row.get(3);
    let time_date_neg: compio_types::Date<time::Date> = row.get(4);
    let time_date_pos: compio_types::Date<time::Date> = row.get(5);
    let time_timestamp_neg: compio_types::Timestamp<time::PrimitiveDateTime> = row.get(6);
    let time_timestamp_pos: compio_types::Timestamp<time::PrimitiveDateTime> = row.get(7);
    let time_timestamptz_neg: compio_types::Timestamp<time::OffsetDateTime> = row.get(8);
    let time_timestamptz_pos: compio_types::Timestamp<time::OffsetDateTime> = row.get(9);
    let time_time_24 = match row.try_get::<_, time::Time>(10) {
        Ok(value) => ValueOutcome::Value(value.to_string()),
        Err(error) if error.code().is_none() => ValueOutcome::LocalFailure,
        Err(error) => ValueOutcome::ServerFailure(error.code().unwrap().code().to_owned()),
    };
    let rebound = client
        .query_one(
            TYPED_TEMPORAL_REBOUND_SQL,
            &[
                &time_date,
                &time_timestamp,
                &time_timestamptz,
                &time_time,
                &time_date_neg,
                &time_date_pos,
                &time_timestamp_neg,
                &time_timestamp_pos,
                &time_timestamptz_neg,
                &time_timestamptz_pos,
            ],
        )
        .await
        .expect("compio time temporal rebound");
    let time_rebound = (0..10).map(|index| rebound.get(index)).collect();

    TypedTemporalObservation {
        chrono_rebound,
        time_rebound,
        chrono_time_24,
        time_time_24,
    }
}

#[cfg(all(feature = "with-chrono-0_4", feature = "with-time-0_3"))]
/// Upstream aliases `PostgreSQL` 24:00 to midnight; this port refuses the loss.
#[compio::test]
async fn time_24_refusal_matches_the_server_instead_of_tokio() {
    let theirs = tokio_typed_temporal_observation(common::plaintext_url());
    let ours = compio_typed_temporal_observation().await;

    let expected = vec![
        "0001-01-01 BC".to_owned(),
        "1999-12-31 23:59:59.999999".to_owned(),
        "2001-02-02 22:20:06.123456+00".to_owned(),
        "23:59:59.999999".to_owned(),
        "-infinity".to_owned(),
        "infinity".to_owned(),
        "-infinity".to_owned(),
        "infinity".to_owned(),
        "-infinity".to_owned(),
        "infinity".to_owned(),
    ];
    assert_eq!(ours.chrono_rebound, expected);
    assert_eq!(ours.time_rebound, expected);
    assert_eq!(theirs.chrono_rebound, expected);
    assert_eq!(theirs.time_rebound, expected);

    assert_eq!(ours.chrono_time_24, ValueOutcome::LocalFailure);
    assert_eq!(ours.time_time_24, ValueOutcome::LocalFailure);
    assert_eq!(
        theirs.chrono_time_24,
        ValueOutcome::Value("00:00:00".to_owned())
    );
    assert_eq!(
        theirs.time_time_24,
        ValueOutcome::Value("0:00:00.0".to_owned())
    );
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
