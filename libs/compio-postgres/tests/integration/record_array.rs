//! Anonymous records are legal array elements in `PostgreSQL` result values.
//!
//! `_record` is a pseudo-type only in the sense that callers cannot use its
//! binary input function without a concrete tuple descriptor. `PostgreSQL` can
//! and does send record arrays: the outer value uses `array_send`, and each
//! element uses `record_send` with its own field OIDs. The driver's type kind
//! must expose that outer array so `Vec<T>` can dispatch each element to a
//! caller-provided record decoder.

use std::error::Error;

use compio_postgres::Client;
use compio_postgres::types::{FromSql, Kind, Type};

#[allow(unused_imports)]
use crate::common;

#[derive(Debug, PartialEq, Eq)]
struct RawRecord(Vec<(u32, Option<Vec<u8>>)>);

fn read_i32(buf: &mut &[u8]) -> Result<i32, Box<dyn Error + Send + Sync>> {
    let bytes: [u8; 4] = buf
        .get(..4)
        .ok_or("short record header")?
        .try_into()
        .expect("the slice length was checked");
    *buf = &buf[4..];
    Ok(i32::from_be_bytes(bytes))
}

impl<'a> FromSql<'a> for RawRecord {
    fn from_sql(_: &Type, mut raw: &'a [u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let field_count = usize::try_from(read_i32(&mut raw)?)?;
        let mut fields = Vec::with_capacity(field_count);

        for _ in 0..field_count {
            let oid = u32::try_from(read_i32(&mut raw)?)?;
            let len = read_i32(&mut raw)?;
            let value = if len == -1 {
                None
            } else {
                let len = usize::try_from(len)?;
                let value = raw.get(..len).ok_or("short record field")?.to_vec();
                raw = &raw[len..];
                Some(value)
            };
            fields.push((oid, value));
        }

        if !raw.is_empty() {
            return Err("trailing record bytes".into());
        }
        Ok(Self(fields))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::RECORD
    }
}

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

/// The built-in `_record` OID must retain its identity while advertising the
/// array structure `PostgreSQL` put on the wire.
#[compio::test]
async fn a_record_array_dispatches_each_record_to_its_element_decoder() {
    let client = connect_client().await;
    let row = client
        .query_one(
            "SELECT ARRAY[ROW(7::int4, 'x'::text), \
             ROW(8::int4, NULL::text)] AS records",
            &[],
        )
        .await
        .expect("PostgreSQL sends an anonymous record array");

    assert_eq!(
        row.columns()[0].type_(),
        &Type::RECORD_ARRAY,
        "the column must keep PostgreSQL's built-in _record OID"
    );
    let records: Vec<RawRecord> = row
        .try_get("records")
        .expect("Vec must dispatch legal record[] elements to RawRecord");
    assert!(
        matches!(
            row.columns()[0].type_().kind(),
            Kind::Array(element) if *element == Type::RECORD
        ),
        "_record must advertise its array element: {:?}",
        row.columns()[0].type_()
    );

    assert_eq!(
        records,
        vec![
            RawRecord(vec![
                (Type::INT4.oid(), Some(7_i32.to_be_bytes().to_vec())),
                (Type::TEXT.oid(), Some(b"x".to_vec())),
            ]),
            RawRecord(vec![
                (Type::INT4.oid(), Some(8_i32.to_be_bytes().to_vec())),
                (Type::TEXT.oid(), None),
            ]),
        ]
    );
}
