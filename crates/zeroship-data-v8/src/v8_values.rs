//! Materialize protected native database values directly in V8.
use zeroship_data_orm::value::Value;
use zeroship_runtime::state::{NativeValue, OpError, ResolveValue};

struct ResultValue {
    value: Value,
    has_masked: bool,
}
impl NativeValue for ResultValue {
    fn into_v8<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let value = encode(scope, self.value)?;
        Ok(if self.has_masked {
            crate::v8_classes::masked_value::rehydrate_masked_values(scope, value).unwrap_or(value)
        } else {
            value
        })
    }
}

pub(crate) fn resolve(value: Value, has_masked: bool) -> ResolveValue {
    ResolveValue::Native(Box::new(ResultValue { value, has_masked }))
}
fn allocation_error() -> OpError {
    OpError::error("could not materialize database result")
}

enum EncodeStep {
    Value(Value),
    Array(usize),
    Object(Vec<String>),
}

pub(crate) fn encode<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let mut pending = vec![EncodeStep::Value(value)];
    let mut values: Vec<v8::Local<'s, v8::Value>> = Vec::new();
    while let Some(step) = pending.pop() {
        match step {
            EncodeStep::Value(value) => values.push(match value {
                Value::Json(encoded) => {
                    let parsed = serde_json::from_str(&encoded)
                        .map_err(|_| OpError::error("invalid JSON value"))?;
                    pending.push(EncodeStep::Value(parsed));
                    continue;
                }
                Value::Array(items) => {
                    pending.push(EncodeStep::Array(items.len()));
                    pending.extend(items.into_iter().rev().map(EncodeStep::Value));
                    continue;
                }
                Value::Object(fields) => {
                    let (keys, children): (Vec<_>, Vec<_>) = fields.into_iter().unzip();
                    pending.push(EncodeStep::Object(keys));
                    pending.extend(children.into_iter().rev().map(EncodeStep::Value));
                    continue;
                }
                Value::Null => v8::null(scope).into(),
                Value::Bool(value) => v8::Boolean::new(scope, value).into(),
                Value::Number(value) => {
                    if let Some(integer) = value
                        .as_i64()
                        .filter(|v| v.unsigned_abs() > 9_007_199_254_740_991)
                    {
                        v8::BigInt::new_from_i64(scope, integer).into()
                    } else if let Some(integer) =
                        value.as_u64().filter(|v| *v > 9_007_199_254_740_991)
                    {
                        v8::BigInt::new_from_u64(scope, integer).into()
                    } else {
                        v8::Number::new(scope, value.as_f64().ok_or_else(allocation_error)?).into()
                    }
                }
                Value::Timestamp(value) => v8::Number::new(scope, value as f64).into(),
                Value::String(value) | Value::Decimal(value) => v8::String::new(scope, &value)
                    .ok_or_else(allocation_error)?
                    .into(),
                Value::Bytes(bytes) => {
                    let length = bytes.len();
                    let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
                    let buffer = v8::ArrayBuffer::with_backing_store(scope, &backing);
                    v8::Uint8Array::new(scope, buffer, 0, length)
                        .ok_or_else(allocation_error)?
                        .into()
                }
            }),
            EncodeStep::Array(length) => {
                let start = values.len() - length;
                let array = v8::Array::new_with_elements(scope, &values[start..]);
                values.truncate(start);
                values.push(array.into());
            }
            EncodeStep::Object(keys) => {
                let start = values.len() - keys.len();
                let object = v8::Object::new(scope);
                for (key, value) in keys.into_iter().zip(&values[start..]) {
                    let key = v8::String::new(scope, &key).ok_or_else(allocation_error)?;
                    if object.create_data_property(scope, key.into(), *value) != Some(true) {
                        return Err(allocation_error());
                    }
                }
                values.truncate(start);
                values.push(object.into());
            }
        }
    }
    values.pop().ok_or_else(allocation_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_limit_leaves_room_for_database_result_envelopes() {
        let mut payload = Value::String("leaf".into());
        for _ in 0..zeroship_data_orm::sql::codecs::MAX_JSON_DEPTH {
            payload = Value::Array(vec![payload]);
        }
        zeroship_data_orm::sql::registration::SqlRegistration::sqlite()
            .encode(
                zeroship_data_orm::sql::statement::StorageType::Json,
                payload.clone(),
            )
            .unwrap();
        let rows = Value::Array(vec![Value::Object([("payload".into(), payload)].into())]);

        zeroship_runtime::init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, Default::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let result = encode(scope, rows).unwrap();
        let rows = v8::Local::<v8::Array>::try_from(result).unwrap();
        let row = rows.get_index(scope, 0).unwrap().to_object(scope).unwrap();
        let key = v8::String::new(scope, "payload").unwrap();
        let mut value = row.get(scope, key.into()).unwrap();
        for _ in 0..zeroship_data_orm::sql::codecs::MAX_JSON_DEPTH {
            value = v8::Local::<v8::Array>::try_from(value)
                .unwrap()
                .get_index(scope, 0)
                .unwrap();
        }
        assert_eq!(value.to_rust_string_lossy(scope), "leaf");
    }

    #[test]
    fn native_values_preserve_bytes_and_integers_without_prototype_callbacks() {
        zeroship_runtime::init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, Default::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let source = v8::String::new(scope, "Object.defineProperty(Array.prototype, '0', {set() { throw new Error('prototype setter'); }, configurable:true}); new Uint8Array([9, 0, 255, 8]).subarray(1,3)").unwrap();
        let script = v8::Script::compile(scope, source, None).unwrap();
        let input = script.run(scope).unwrap();
        let bytes = crate::v8_bridge::decode_native(scope, input).unwrap();
        assert_eq!(bytes.as_bytes(), Some([0, 255].as_slice()));
        let native = Value::Array(vec![bytes, Value::from(i64::MAX), Value::from(u64::MAX)]);
        let output = encode(scope, native.clone()).unwrap();
        let decoded = crate::v8_bridge::decode_native(scope, output).unwrap();
        assert_eq!(decoded, native);
        let array = v8::Local::<v8::Array>::try_from(output).unwrap();
        assert!(array.get_index(scope, 0).unwrap().is_uint8_array());
        assert!(array.get_index(scope, 1).unwrap().is_big_int());
        let native = Value::Object([("__proto__".into(), Value::from("ordinary field"))].into());
        let output = encode(scope, native.clone()).unwrap();
        assert_eq!(
            crate::v8_bridge::decode_native(scope, output).unwrap(),
            native
        );
    }

    #[test]
    fn encoded_results_obey_json_validation() {
        zeroship_runtime::init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, Default::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let depth = zeroship_data_orm::sql::codecs::MAX_JSON_DEPTH + 1;
        for encoded in [
            "private-invalid-json".to_owned(),
            format!("{}null{}", "[".repeat(depth), "]".repeat(depth)),
        ] {
            let error = encode(scope, Value::Json(encoded)).unwrap_err();
            assert_eq!(error.message, "invalid JSON value");
        }
    }
}
