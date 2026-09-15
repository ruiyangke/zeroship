//! Encode native database values at the PostgreSQL protocol boundary.
use crate::value::Value;
use compio_postgres::types::{private::BytesMut, Format, IsNull, Kind, ToSql, Type};
type EncodeError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub struct Parameter<'a>(pub &'a Value);

/// A one-dimensional array whose elements use the text binary format.
fn is_text_array(ty: &Type) -> bool {
    matches!(ty.kind(), Kind::Array(member) if <&str as ToSql>::accepts(member))
}

impl ToSql for Parameter<'_> {
    fn accepts(_: &Type) -> bool {
        true
    }
    fn encode_format(&self, ty: &Type) -> Format {
        if matches!(self.0, Value::Object(_)) && ty.name() == "geography" {
            return Format::Binary;
        }
        if matches!(self.0, Value::Array(_)) && is_text_array(ty) {
            return Format::Binary;
        }
        match (self.0, ty) {
            (Value::Null, _) | (Value::Bytes(_), &Type::BYTEA) | (Value::Bool(_), &Type::BOOL) => {
                Format::Binary
            }
            (Value::TimestampMicros(_), &Type::TIMESTAMP | &Type::TIMESTAMPTZ) => Format::Binary,
            (Value::Number(n), &Type::INT2 | &Type::INT4 | &Type::INT8) if n.as_i64().is_some() => {
                Format::Binary
            }
            (Value::Number(_), &Type::FLOAT4 | &Type::FLOAT8) => Format::Binary,
            _ => Format::Text,
        }
    }
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, EncodeError> {
        if let Value::Object(point) = self.0 {
            if ty.name() == "geography" {
                let lat = point
                    .get("lat")
                    .and_then(Value::as_f64)
                    .ok_or("geographic point requires numeric latitude")?;
                let lng = point
                    .get("lng")
                    .and_then(Value::as_f64)
                    .ok_or("geographic point requires numeric longitude")?;
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
                    return Err("geographic point coordinates are out of range".into());
                }
                // EWKB point with an explicit WGS84 SRID, longitude then latitude.
                out.extend_from_slice(&[1]);
                out.extend_from_slice(&0x2000_0001u32.to_le_bytes());
                out.extend_from_slice(&4326u32.to_le_bytes());
                out.extend_from_slice(&lng.to_le_bytes());
                out.extend_from_slice(&lat.to_le_bytes());
                return Ok(IsNull::No);
            }
        }
        if let Value::Array(values) = self.0 {
            if is_text_array(ty) {
                // The binary array protocol preserves order, duplicates, empty
                // strings and the text "NULL" exactly.
                let elements = values
                    .iter()
                    .map(|value| match value {
                        Value::String(text) if !text.contains('\0') => Ok(text.as_str()),
                        _ => Err("text array parameters require strings without NUL"),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                return elements.to_sql(ty, out);
            }
            if ty.name() == "vector" {
                out.extend_from_slice(b"[");
                for (index, value) in values.iter().enumerate() {
                    let component =
                        value.as_f64().ok_or("vector components must be numbers")? as f32;
                    if !component.is_finite() {
                        return Err("vector component exceeds the column range".into());
                    }
                    if index != 0 {
                        out.extend_from_slice(b",");
                    }
                    out.extend_from_slice(component.to_string().as_bytes());
                }
                out.extend_from_slice(b"]");
                return Ok(IsNull::No);
            }
        }
        if let Value::Json(json) = self.0 {
            if !matches!(*ty, Type::JSON | Type::JSONB) {
                return Err("JSON parameter requires a JSON column".into());
            }
            out.extend_from_slice(json.as_bytes());
            return Ok(IsNull::No);
        }
        if matches!(*ty, Type::JSON | Type::JSONB) && !self.0.is_null() {
            out.extend_from_slice(&serde_json::to_vec(self.0)?);
            return Ok(IsNull::No);
        }
        match (self.0, ty) {
            (Value::Null, _) => return Ok(IsNull::Yes),
            (Value::Bytes(v), &Type::BYTEA) => out.extend_from_slice(v),
            (Value::Bytes(_), _) => return Err("binary parameter requires a bytea column".into()),
            (Value::Bool(v), &Type::BOOL) => out.extend_from_slice(&[u8::from(*v)]),
            (Value::Number(n), &Type::INT2) if n.as_i64().is_some() => {
                out.extend_from_slice(&i16::try_from(n.as_i64().unwrap())?.to_be_bytes())
            }
            (Value::Number(n), &Type::INT4) if n.as_i64().is_some() => {
                out.extend_from_slice(&i32::try_from(n.as_i64().unwrap())?.to_be_bytes())
            }
            (Value::Number(n), &Type::INT8) if n.as_i64().is_some() => {
                out.extend_from_slice(&n.as_i64().unwrap().to_be_bytes())
            }
            (Value::Number(n), &Type::FLOAT8) => {
                out.extend_from_slice(&n.as_f64().ok_or("invalid float")?.to_be_bytes())
            }
            (Value::Number(n), &Type::FLOAT4) => {
                let v = n.as_f64().ok_or("invalid float")? as f32;
                if !v.is_finite() {
                    return Err("float parameter exceeds the column range".into());
                }
                out.extend_from_slice(&v.to_be_bytes());
            }
            (Value::String(v) | Value::Decimal(v) | Value::Json(v), _) => {
                out.extend_from_slice(v.as_bytes())
            }
            (Value::Number(v), _) => out.extend_from_slice(v.to_string().as_bytes()),
            (Value::Bool(v), _) => out.extend_from_slice(if *v { b"true" } else { b"false" }),
            // The exact inverse of the decode: rebase on 2000-01-01 and send
            // every microsecond.
            (Value::TimestampMicros(v), &Type::TIMESTAMP | &Type::TIMESTAMPTZ) => {
                let micros = v
                    .checked_sub(super::pg_row_json::POSTGRES_EPOCH_UNIX_MICROS)
                    .ok_or("timestamp exceeds PostgreSQL range")?;
                out.extend_from_slice(&micros.to_be_bytes());
            }
            (Value::TimestampMicros(v), _) => out.extend_from_slice(v.to_string().as_bytes()),
            (Value::Array(_) | Value::Object(_), &Type::JSON | &Type::JSONB) => {
                out.extend_from_slice(&serde_json::to_vec(self.0)?)
            }
            (Value::Array(_) | Value::Object(_), _) => {
                return Err("structured parameter requires a JSON column".into());
            }
        }
        Ok(IsNull::No)
    }
    compio_postgres::types::to_sql_checked!();
}

pub async fn query(
    client: &compio_postgres::Client,
    sql: &str,
    values: &[Value],
) -> Result<Vec<compio_postgres::Row>, zeroship_data_orm::error::DbError> {
    let parameters: Vec<_> = values.iter().map(Parameter).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = parameters.iter().map(|p| p as _).collect();
    client
        .query(sql, &refs)
        .await
        .map_err(|e| crate::backend::postgres::pg_error::classify(&e))
}

/// Execute a parameterized command and return its command-completion row count.
pub async fn execute(
    client: &compio_postgres::Client,
    sql: &str,
    values: &[Value],
) -> Result<u64, crate::error::DbError> {
    let parameters: Vec<_> = values.iter().map(Parameter).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = parameters.iter().map(|value| value as _).collect();
    client
        .execute(sql, &refs)
        .await
        .map_err(|e| super::pg_error::classify(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_parameters_require_finite_numeric_components() {
        let vector = Type::new(
            "vector".into(),
            16384,
            compio_postgres::types::Kind::Simple,
            "public".into(),
        );
        let value = crate::value!([1.0, 0.5]);
        let mut bytes = BytesMut::new();
        Parameter(&value).to_sql(&vector, &mut bytes).unwrap();
        assert_eq!(bytes.as_ref(), b"[1,0.5]");
        for value in [
            crate::value!(["1"]),
            Value::Array(vec![Value::try_from(f64::MAX).unwrap()]),
        ] {
            assert!(Parameter(&value)
                .to_sql(&vector, &mut BytesMut::new())
                .is_err());
        }
    }

    #[test]
    fn parameters_preserve_scalar_types_and_check_integer_widths() {
        let mut bytes = BytesMut::new();
        let integer = Value::from(i64::MAX);
        let param = Parameter(&integer);
        assert!(matches!(param.encode_format(&Type::INT8), Format::Binary));
        param.to_sql(&Type::INT8, &mut bytes).unwrap();
        assert_eq!(bytes.as_ref(), &i64::MAX.to_be_bytes());
        assert!(param.to_sql(&Type::INT4, &mut BytesMut::new()).is_err());
        let raw = Value::Bytes(vec![0, 255, 128]);
        let param = Parameter(&raw);
        bytes.clear();
        param.to_sql(&Type::BYTEA, &mut bytes).unwrap();
        assert_eq!(bytes.as_ref(), raw.as_bytes().unwrap());
        assert!(param.to_sql(&Type::TEXT, &mut BytesMut::new()).is_err());
        bytes.clear();
        Parameter(&Value::from("quoted"))
            .to_sql(&Type::JSONB, &mut bytes)
            .unwrap();
        assert_eq!(bytes.as_ref(), br#""quoted""#);
        assert!(matches!(
            Parameter(&Value::Null)
                .to_sql(&Type::TEXT, &mut bytes)
                .unwrap(),
            IsNull::Yes
        ));
    }

    #[test]
    fn text_array_parameters_encode_binary_arrays() {
        for items in [
            vec![],
            vec!["only"],
            vec!["dup", "dup"],
            vec!["NULL", "", "{\"q\",\\x} é"],
        ] {
            let value = Value::Array(items.iter().copied().map(Value::from).collect());
            let parameter = Parameter(&value);
            assert!(matches!(
                parameter.encode_format(&Type::TEXT_ARRAY),
                Format::Binary
            ));
            let mut encoded = BytesMut::new();
            assert!(matches!(
                parameter.to_sql(&Type::TEXT_ARRAY, &mut encoded).unwrap(),
                IsNull::No
            ));
            let mut expected = BytesMut::new();
            items.to_sql(&Type::TEXT_ARRAY, &mut expected).unwrap();
            assert_eq!(encoded.as_ref(), expected.as_ref(), "{items:?}");
        }
        for value in [
            crate::value!([1]),
            crate::value!(["a", null]),
            crate::value!([["nested"]]),
            crate::value!(["nul\u{0}"]),
        ] {
            assert!(Parameter(&value)
                .to_sql(&Type::TEXT_ARRAY, &mut BytesMut::new())
                .is_err());
        }
        let strings = crate::value!(["a"]);
        for ty in [Type::TEXT, Type::INT4_ARRAY] {
            assert!(Parameter(&strings)
                .to_sql(&ty, &mut BytesMut::new())
                .is_err());
        }
        let mut json = BytesMut::new();
        Parameter(&strings).to_sql(&Type::JSONB, &mut json).unwrap();
        assert_eq!(json.as_ref(), br#"["a"]"#);
        assert!(matches!(
            Parameter(&strings).encode_format(&Type::JSONB),
            Format::Text
        ));
        let raw = Value::Bytes(vec![0, 1]);
        assert!(matches!(
            Parameter(&raw).encode_format(&Type::BYTEA),
            Format::Binary
        ));
    }

    #[compio::test]
    async fn text_array_parameters_match_a_server_oracle() {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let (client, connection) =
            compio_postgres::connect(&postgres.url(), compio_postgres::NoTls)
                .await
                .unwrap();
        compio::runtime::spawn(async move {
            connection.run().await.unwrap();
        })
        .detach();
        let unusual = "{\"q\",\\x} é , NULL";
        for (items, cardinality, lower) in [
            (vec![], 0, None),
            (vec!["b", "a", "a"], 3, Some(1)),
            (vec!["NULL"], 1, Some(1)),
            (vec![""], 1, Some(1)),
            (vec![unusual, ","], 2, Some(1)),
        ] {
            let array = Value::Array(items.iter().copied().map(Value::from).collect());
            let first = Value::from(items.first().copied().unwrap_or(""));
            let rows = query(
                &client,
                "SELECT cardinality($1::text[]) AS cardinality, \
                 array_lower($1::text[], 1) AS lower, \
                 array_position($1::text[], NULL) IS NULL AS no_null_element, \
                 coalesce(($1::text[])[1] = $2::text, false) AS first_matches, \
                 $1::text[] = ARRAY['b','a','a']::text[] AS ordered, \
                 $1::text[] IS NULL AS absent, \
                 $1::text[] AS labels",
                &[array.clone(), first],
            )
            .await
            .unwrap();
            let row = &crate::backend::postgres::pg_row_json::rows_to_values(&rows).unwrap()[0];
            assert_eq!(row["cardinality"], Value::from(cardinality), "{items:?}");
            assert_eq!(row["lower"], lower.map_or(Value::Null, Value::from));
            assert_eq!(row["no_null_element"], Value::Bool(true));
            assert_eq!(row["first_matches"], Value::Bool(!items.is_empty()));
            assert_eq!(row["ordered"], Value::Bool(items == ["b", "a", "a"]));
            assert_eq!(row["absent"], Value::Bool(false));
            assert_eq!(row["labels"], array);
        }
        let rows = query(
            &client,
            "SELECT $1::text[] IS NULL AS absent",
            &[Value::Null],
        )
        .await
        .unwrap();
        assert_eq!(
            crate::backend::postgres::pg_row_json::rows_to_values(&rows).unwrap()[0]["absent"],
            Value::Bool(true)
        );
    }

    #[compio::test]
    async fn native_values_round_trip_through_postgres() {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let (client, connection) =
            compio_postgres::connect(&postgres.url(), compio_postgres::NoTls)
                .await
                .unwrap();
        compio::runtime::spawn(async move {
            connection.run().await.unwrap();
        })
        .detach();
        let values = [
            Value::from(i64::MAX),
            Value::Bytes(vec![0, 255, 128]),
            Value::Bool(true),
            Value::Null,
            Value::from("text"),
            crate::value!({"nested":[1, false]}),
            Value::Decimal("12345678901234567890.12345678901234567890".into()),
            Value::try_from(1.25).unwrap(),
            Value::TimestampMicros(-1),
        ];
        let rows = query(&client, "SELECT $1::bigint AS integer, $2::bytea AS bytes, $3::boolean AS flag, $4::text AS absent, $5::text AS text, $6::jsonb AS document, $7::numeric AS decimal, $8::double precision AS number, $9::timestamptz AS stamp", &values).await.unwrap();
        let native = crate::backend::postgres::pg_row_json::rows_to_values(&rows).unwrap();
        for (field, value) in [
            "integer", "bytes", "flag", "absent", "text", "document", "decimal", "number", "stamp",
        ]
        .into_iter()
        .zip(values)
        {
            assert_eq!(native[0][field], value, "{field}");
        }
    }
    #[compio::test]
    async fn json_filter_literals_are_not_encoded_as_strings() {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        use crate::value;
        let (client, connection) =
            compio_postgres::connect(&postgres.url(), compio_postgres::NoTls)
                .await
                .unwrap();
        compio::runtime::spawn(async move {
            connection.run().await.unwrap();
        })
        .detach();
        let params = [value!({"key":"value"})];
        let rows = query(&client, "SELECT $1::jsonb AS payload", &params)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            crate::backend::postgres::pg_row_json::rows_to_values(&rows).unwrap()[0]["payload"],
            value!({"key":"value"})
        );
    }
}
