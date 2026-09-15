//! Decode PostgreSQL binary rows into native ORM values.
//!
//! SQL NULL remains distinct from a malformed or unsupported value. Decode
//! failures propagate to Rust and V8 callers with column context.

use crate::error::DbError;
use compio_postgres::{
    Row,
    types::{FromSql, Kind, Type},
};
use crate::value::{Map, Value};

#[cfg(test)]
#[path = "pg_row_json/array_tests.rs"]
mod array_tests;
#[cfg(test)]
#[path = "pg_row_json/network_tests.rs"]
mod network_tests;

pub fn rows_to_values(rows: &[Row]) -> Result<Vec<Value>, DbError> {
    rows.iter().map(row_to_value).collect()
}

pub(crate) fn row_to_value(row: &Row) -> Result<Value, DbError> {
    let mut object = Map::with_capacity(row.columns().len());
    for (index, column) in row.columns().iter().enumerate() {
        let error = |reason: &str| DbError::row_decode(column.name(), reason);
        let value = match row
            .raw_value(index)
            .map_err(|_| error("invalid row layout"))?
        {
            None => Value::Null,
            Some(bytes) => decode_value(column.type_(), bytes).map_err(|reason| error(&reason))?,
        };
        object.insert(column.name().to_owned(), value);
    }
    Ok(Value::Object(object))
}

fn from_sql<'a, T: FromSql<'a>>(ty: &Type, bytes: &'a [u8]) -> Result<T, String> {
    T::from_sql(ty, bytes).map_err(|_| format!("invalid {} binary value", ty.name()))
}

fn finite_number(value: f64) -> Result<Value, String> {
    Value::try_from(value).map_err(|_| "non-finite numbers are unsupported".into())
}

fn decode_value(ty: &Type, bytes: &[u8]) -> Result<Value, String> {
    match ty.kind() {
        Kind::Domain(base) => return decode_value(base, bytes),
        Kind::Enum(_) => {
            return std::str::from_utf8(bytes)
                .map(Value::from)
                .map_err(|_| "invalid enum text".into());
        }
        Kind::Array(member) if <String as FromSql>::accepts(member) => {
            return decode_text_array(member, bytes);
        }
        _ => {}
    }
    match *ty {
        Type::BYTEA => Ok(Value::Bytes(bytes.to_vec())),
        Type::BOOL => match bytes {
            [0] => Ok(Value::Bool(false)),
            [1] => Ok(Value::Bool(true)),
            _ => Err("invalid boolean binary value".into()),
        },
        Type::INT2 => from_sql::<i16>(ty, bytes).map(Value::from),
        Type::INT4 => from_sql::<i32>(ty, bytes).map(Value::from),
        Type::INT8 => from_sql::<i64>(ty, bytes).map(Value::from),
        Type::OID => from_sql::<u32>(ty, bytes).map(Value::from),
        Type::CHAR => from_sql::<i8>(ty, bytes).map(Value::from),
        Type::FLOAT4 => finite_number(f64::from(from_sql::<f32>(ty, bytes)?)),
        Type::FLOAT8 => finite_number(from_sql::<f64>(ty, bytes)?),
        Type::UUID => from_sql::<uuid::Uuid>(ty, bytes).map(|value| Value::from(value.to_string())),
        Type::INET | Type::CIDR => decode_network(ty, bytes),
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let micros = i64::from_be_bytes(
                bytes
                    .try_into()
                    .map_err(|_| "invalid timestamp binary value")?,
            );
            if matches!(micros, i64::MIN | i64::MAX) {
                return Err("infinite timestamps are unsupported".into());
            }
            // Floor to the containing millisecond on both sides of the epoch.
            Ok(Value::Timestamp(micros.div_euclid(1000) + 946_684_800_000))
        }
        Type::DATE => {
            let days =
                i32::from_be_bytes(bytes.try_into().map_err(|_| "invalid date binary value")?);
            if matches!(days, i32::MIN | i32::MAX) {
                return Err("infinite dates are unsupported".into());
            }
            days.checked_add(10957)
                .and_then(crate::sql::temporal::format_calendar_date)
                .map(Value::String)
                .ok_or_else(|| "calendar date is outside the supported range".into())
        }
        Type::JSONB => {
            let Some((&1, json)) = bytes.split_first() else {
                return Err("unsupported JSONB binary version".into());
            };
            serde_json::from_slice(json).map_err(|_| "invalid JSONB value".into())
        }
        Type::JSON => serde_json::from_slice(bytes).map_err(|_| "invalid JSON value".into()),
        Type::NUMERIC => decode_numeric(bytes)
            .map(Value::Decimal)
            .ok_or_else(|| "invalid or non-finite numeric value".into()),
        _ if ty.name() == "vector" => decode_vector(bytes),
        _ if ty.name() == "geography" => decode_geography(bytes),
        _ if <String as FromSql>::accepts(ty) => from_sql::<String>(ty, bytes).map(Value::from),
        _ => Err(format!("unsupported PostgreSQL type '{}'", ty.name())),
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn word(&mut self) -> Result<[u8; 4], String> {
        let (word, rest) = self
            .0
            .split_first_chunk::<4>()
            .ok_or("truncated array binary value")?;
        self.0 = rest;
        Ok(*word)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        if length > self.0.len() {
            return Err("truncated array binary value".into());
        }
        let (taken, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(taken)
    }
}

/// PostgreSQL's binary array layout, accepted only for the shapes a text array
/// column stores: no dimension when empty, otherwise one dimension with the
/// default lower bound. Other dimensions and bounds change array equality, so
/// they are refused rather than flattened. A NULL element stays `Value::Null`.
fn decode_text_array(member: &Type, bytes: &[u8]) -> Result<Value, String> {
    let mut reader = Reader(bytes);
    let dimensions = i32::from_be_bytes(reader.word()?);
    let has_nulls = match i32::from_be_bytes(reader.word()?) {
        0 => false,
        1 => true,
        _ => return Err("invalid array flags".into()),
    };
    if u32::from_be_bytes(reader.word()?) != member.oid() {
        return Err("array element type does not match the column".into());
    }
    let length = match dimensions {
        0 => 0,
        1 => {
            let length = i32::from_be_bytes(reader.word()?);
            if i32::from_be_bytes(reader.word()?) != 1 {
                return Err("arrays with a non-default lower bound are unsupported".into());
            }
            usize::try_from(length)
                .ok()
                .filter(|length| *length > 0)
                .ok_or("invalid array length")?
        }
        _ => return Err("only one-dimensional arrays are supported".into()),
    };
    let mut values = Vec::with_capacity(length.min(reader.0.len() / 4));
    for _ in 0..length {
        let size = i32::from_be_bytes(reader.word()?);
        if size == -1 {
            if !has_nulls {
                return Err("array null element without a null flag".into());
            }
            values.push(Value::Null);
            continue;
        }
        let size = usize::try_from(size).map_err(|_| "invalid array element length")?;
        values.push(from_sql::<String>(member, reader.take(size)?).map(Value::from)?);
    }
    if !reader.0.is_empty() {
        return Err("trailing bytes after array elements".into());
    }
    Ok(Value::Array(values))
}

fn decode_network(ty: &Type, bytes: &[u8]) -> Result<Value, String> {
    let is_cidr = *ty == Type::CIDR;
    if bytes.get(2).copied() != Some(u8::from(is_cidr)) {
        return Err(format!("invalid {} binary value", ty.name()));
    }
    if is_cidr {
        from_sql::<cidr::IpCidr>(ty, bytes).map(|value| Value::String(format!("{value:#}")))
    } else {
        from_sql::<cidr::IpInet>(ty, bytes).map(|value| Value::String(value.to_string()))
    }
}

/// pgvector's binary send format: dimensions, reserved word, then components.
fn decode_vector(bytes: &[u8]) -> Result<Value, String> {
    let Some(header) = bytes.get(..4) else {
        return Err("invalid vector header".into());
    };
    let dimensions = i16::from_be_bytes([header[0], header[1]]);
    if dimensions <= 0 || header[2..] != [0, 0] || bytes.len() != 4 + dimensions as usize * 4 {
        return Err("invalid vector dimensions or binary layout".into());
    }
    bytes[4..]
        .chunks_exact(4)
        .map(|chunk| finite_number(f64::from(f32::from_be_bytes(chunk.try_into().unwrap()))))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// PostGIS geography sends EWKB. The ORM's geographic type is a WGS84 point.
fn decode_geography(bytes: &[u8]) -> Result<Value, String> {
    let Some(header) = bytes.get(..5) else {
        return Err("invalid geography header".into());
    };
    let little_endian = match header[0] {
        0 => false,
        1 => true,
        _ => return Err("invalid geography byte order".into()),
    };
    let word = |bytes: &[u8]| {
        let word = bytes.try_into().unwrap();
        if little_endian {
            u32::from_le_bytes(word)
        } else {
            u32::from_be_bytes(word)
        }
    };
    let coordinate_offset = match word(&header[1..]) {
        1 => 5,
        0x2000_0001 => {
            let srid = bytes.get(5..9).ok_or("missing geography SRID")?;
            if word(srid) != 4326 {
                return Err("geographic points require WGS84 coordinates".into());
            }
            9
        }
        _ => return Err("only geographic points are supported".into()),
    };
    if bytes.len() != coordinate_offset + 16 {
        return Err("invalid geographic point binary layout".into());
    }
    let coordinate = |bytes: &[u8]| {
        let value = bytes.try_into().unwrap();
        if little_endian {
            f64::from_le_bytes(value)
        } else {
            f64::from_be_bytes(value)
        }
    };
    let lng = coordinate(&bytes[coordinate_offset..coordinate_offset + 8]);
    let lat = coordinate(&bytes[coordinate_offset + 8..]);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err("invalid geographic point coordinates".into());
    }
    Ok(Value::Object(
        [
            ("lat".into(), finite_number(lat)?),
            ("lng".into(), finite_number(lng)?),
        ]
        .into(),
    ))
}

/// Decode finite PostgreSQL base-group numeric storage without floating point.
fn decode_numeric(bytes: &[u8]) -> Option<String> {
    use std::fmt::Write;
    if bytes.len() < 8 {
        return None;
    }
    let word = |offset| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
    let count = usize::from(word(0));
    let weight = i32::from(word(2) as i16);
    let sign = word(4);
    let scale = usize::from(word(6));
    if bytes.len() != 8 + count * 2 || !matches!(sign, 0 | 0x4000) || scale > 16383 {
        return None;
    }
    let digits: Vec<_> = (0..count).map(|i| word(8 + i * 2)).collect();
    if digits.iter().any(|d| *d >= 10000) {
        return None;
    }
    let digit = |position: i32| {
        usize::try_from(weight - position)
            .ok()
            .and_then(|i| digits.get(i))
            .copied()
            .unwrap_or(0)
    };
    let mut result = String::new();
    if sign == 0x4000 && digits.iter().any(|d| *d != 0) {
        result.push('-');
    }
    if weight < 0 {
        result.push('0');
    } else {
        write!(result, "{}", digit(weight)).ok()?;
        for position in (0..weight).rev() {
            write!(result, "{:04}", digit(position)).ok()?;
        }
    }
    if scale > 0 {
        result.push('.');
        let start = result.len();
        for index in 1..=scale.div_ceil(4) {
            write!(result, "{:04}", digit(-(index as i32))).ok()?;
        }
        result.truncate(start + scale);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compio_postgres::test_utils::{column_for_test, row_for_test};

    #[test]
    fn calendar_dates_decode_as_dates_without_a_time_component() {
        let row = row_for_test(
            vec![column_for_test("birthday", Type::DATE)],
            vec![Some(0i32.to_be_bytes().to_vec())],
        )
        .unwrap();
        assert_eq!(
            row_to_value(&row).unwrap()["birthday"],
            Value::String("2000-01-01".into())
        );
    }

    #[test]
    fn malformed_cells_fail_with_column_context_without_echoing_values() {
        let payloads = [
            (Type::INT4, vec![0]),
            (Type::BOOL, vec![2]),
            (Type::FLOAT8, vec![]),
            (Type::UUID, vec![0]),
            (Type::TIMESTAMP, vec![0]),
            (Type::DATE, vec![0]),
            (Type::JSONB, b"\x02null".to_vec()),
            (Type::JSONB, b"\x01secret_not_json".to_vec()),
            (Type::JSON, b"secret_not_json".to_vec()),
            (Type::TEXT, vec![255]),
            (Type::NUMERIC, vec![0]),
        ];
        for (ty, bytes) in payloads {
            let row = row_for_test(
                vec![column_for_test("invalid_result", ty)],
                vec![Some(bytes)],
            )
            .unwrap();
            let error = row_to_value(&row).unwrap_err();
            let DbError::Coded { code, message, .. } = error else {
                panic!("{error}")
            };
            assert_eq!(code, "row_decode_failed");
            assert!(message.contains("invalid_result"));
            assert!(!message.contains("secret_not_json"));
        }
        let valid = row_for_test(
            vec![column_for_test("item", Type::TEXT)],
            vec![Some(b"visible".to_vec())],
        )
        .unwrap();
        let broken = row_for_test(
            vec![column_for_test("item", Type::INT4)],
            vec![Some(vec![0])],
        )
        .unwrap();
        assert!(
            rows_to_values(&[valid, broken]).is_err(),
            "a failed row cannot become a partial successful batch"
        );
    }

    #[test]
    fn extension_binary_values_validate_their_entire_layout() {
        let vector = [0, 2, 0, 0]
            .into_iter()
            .chain(1.0f32.to_be_bytes())
            .chain((-0.5f32).to_be_bytes())
            .collect::<Vec<_>>();
        assert_eq!(
            decode_vector(&vector).unwrap(),
            crate::value!([1.0, -0.5])
        );
        for length in 0..vector.len() {
            assert!(decode_vector(&vector[..length]).is_err());
        }
        let mut nonfinite = vector.clone();
        nonfinite[4..8].copy_from_slice(&f32::INFINITY.to_be_bytes());
        assert!(decode_vector(&nonfinite).is_err());
        let mut reserved = vector.clone();
        reserved[3] = 1;
        assert!(decode_vector(&reserved).is_err());

        for little_endian in [false, true] {
            let word = |value: u32| {
                if little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                }
            };
            let coordinate = |value: f64| {
                if little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                }
            };
            let point = [u8::from(little_endian)]
                .into_iter()
                .chain(word(0x2000_0001))
                .chain(word(4326))
                .chain(coordinate(-122.0))
                .chain(coordinate(37.0))
                .collect::<Vec<_>>();
            assert_eq!(
                decode_geography(&point).unwrap(),
                crate::value!({"lat":37.0, "lng":-122.0})
            );
            for length in 0..point.len() {
                assert!(decode_geography(&point[..length]).is_err());
            }
            let mut invalid = point.clone();
            invalid[9..17].copy_from_slice(&coordinate(f64::NAN));
            assert!(decode_geography(&invalid).is_err());
            let mut wrong_srid = point.clone();
            wrong_srid[5..9].copy_from_slice(&word(3857));
            assert!(decode_geography(&wrong_srid).is_err());
            let mut wrong_type = point.clone();
            wrong_type[1..5].copy_from_slice(&word(0x2000_0002));
            assert!(decode_geography(&wrong_type).is_err());
            let mut trailing = point;
            trailing.push(0);
            assert!(decode_geography(&trailing).is_err());
        }
    }
}
