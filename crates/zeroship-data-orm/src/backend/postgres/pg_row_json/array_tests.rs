use super::{decode_value, row_to_value, DbError, Type, Value};
use compio_postgres::test_utils::{column_for_test, row_for_test};
use compio_postgres::types::{private::BytesMut, ToSql};

const TEXT_OID: u32 = 25;
const INT4_OID: u32 = 23;
const VARCHAR_OID: u32 = 1043;

fn array(dimensions: &[(i32, i32)], flags: i32, element: u32, items: &[Option<&[u8]>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(i32::try_from(dimensions.len()).unwrap().to_be_bytes());
    bytes.extend(flags.to_be_bytes());
    bytes.extend(element.to_be_bytes());
    for (length, lower_bound) in dimensions {
        bytes.extend(length.to_be_bytes());
        bytes.extend(lower_bound.to_be_bytes());
    }
    for item in items {
        match item {
            None => bytes.extend((-1_i32).to_be_bytes()),
            Some(item) => {
                bytes.extend(i32::try_from(item.len()).unwrap().to_be_bytes());
                bytes.extend(*item);
            }
        }
    }
    bytes
}

fn text_array(items: &[Option<&str>]) -> Vec<u8> {
    let items: Vec<_> = items.iter().map(|item| item.map(str::as_bytes)).collect();
    let dimensions = if items.is_empty() {
        vec![]
    } else {
        vec![(i32::try_from(items.len()).unwrap(), 1)]
    };
    let flags = i32::from(items.iter().any(Option::is_none));
    array(&dimensions, flags, TEXT_OID, &items)
}

fn strings(items: &[&str]) -> Value {
    Value::Array(items.iter().copied().map(Value::from).collect())
}

fn refusal(ty: Type, bytes: Vec<u8>) -> String {
    let row = row_for_test(
        vec![column_for_test("secret_labels", ty)],
        vec![Some(bytes)],
    )
    .unwrap();
    let error = row_to_value(&row).unwrap_err();
    let DbError::Coded { code, message, .. } = error else {
        panic!("{error}")
    };
    assert_eq!(code, "row_decode_failed");
    assert!(message.contains("secret_labels"), "{message}");
    assert!(!message.contains("private_element"), "{message}");
    message
}

#[test]
fn text_array_binary_decodes_exactly() {
    let unusual = "{\"private_element\",\\x} é 🦀 , 'q'";
    for (items, expected) in [
        (vec![], Value::Array(vec![])),
        (
            vec![Some("b"), Some("a"), Some("a")],
            strings(&["b", "a", "a"]),
        ),
        (vec![Some("NULL")], strings(&["NULL"])),
        (vec![None], Value::Array(vec![Value::Null])),
        (
            vec![Some("a"), None, Some("NULL")],
            Value::Array(vec![Value::from("a"), Value::Null, Value::from("NULL")]),
        ),
        (vec![Some("")], strings(&[""])),
        (vec![Some(unusual), Some(",")], strings(&[unusual, ","])),
    ] {
        assert_eq!(
            decode_value(&Type::TEXT_ARRAY, &text_array(&items)).unwrap(),
            expected,
            "{items:?}"
        );
    }
    let varchar = array(&[(1, 1)], 0, VARCHAR_OID, &[Some(b"short")]);
    assert_eq!(
        decode_value(&Type::VARCHAR_ARRAY, &varchar).unwrap(),
        strings(&["short"])
    );
    let row = row_for_test(
        vec![column_for_test("labels", Type::TEXT_ARRAY)],
        vec![None],
    )
    .unwrap();
    assert_eq!(row_to_value(&row).unwrap()["labels"], Value::Null);
}

#[test]
fn text_array_binary_agrees_with_the_driver_encoder() {
    for items in [
        vec![],
        vec![Some("only")],
        vec![Some("dup"), Some("dup")],
        vec![Some("NULL"), None, Some("")],
    ] {
        let mut encoded = BytesMut::new();
        items.to_sql(&Type::TEXT_ARRAY, &mut encoded).unwrap();
        let expected = Value::Array(
            items
                .iter()
                .map(|item| item.map_or(Value::Null, Value::from))
                .collect(),
        );
        assert_eq!(decode_value(&Type::TEXT_ARRAY, &encoded).unwrap(), expected);
        assert_eq!(encoded.as_ref(), text_array(&items));
    }
}

#[test]
fn malformed_text_array_binary_is_refused_with_column_context() {
    let valid = text_array(&[Some("private_element"), Some("b")]);
    let mut malformed: Vec<Vec<u8>> = (0..valid.len())
        .map(|length| valid[..length].to_vec())
        .collect();
    let mut trailing = valid.clone();
    trailing.push(0);
    malformed.push(trailing);
    malformed.extend([
        array(&[(1, 1), (1, 1)], 0, TEXT_OID, &[Some(b"private_element")]),
        {
            let mut negative = text_array(&[]);
            negative[..4].copy_from_slice(&(-1_i32).to_be_bytes());
            negative
        },
        array(&[(1, 0)], 0, TEXT_OID, &[Some(b"private_element")]),
        array(&[(1, 2)], 0, TEXT_OID, &[Some(b"private_element")]),
        array(&[(0, 1)], 0, TEXT_OID, &[]),
        array(&[(-1, 1)], 0, TEXT_OID, &[]),
        array(&[(1, 1)], 0, INT4_OID, &[Some(&7_i32.to_be_bytes())]),
        array(&[(1, 1)], 2, TEXT_OID, &[Some(b"private_element")]),
        array(&[(1, 1)], 0, TEXT_OID, &[None]),
        array(&[(2, 1)], 0, TEXT_OID, &[Some(b"private_element")]),
        array(&[(1, 1)], 0, TEXT_OID, &[Some(&[0xff, 0xfe])]),
        {
            let mut short_length = array(&[(1, 1)], 0, TEXT_OID, &[Some(b"private_element")]);
            let length = short_length.len() - b"private_element".len() - 4;
            short_length[length..length + 4].copy_from_slice(&(-2_i32).to_be_bytes());
            short_length
        },
        {
            let mut long_length = array(&[(1, 1)], 0, TEXT_OID, &[Some(b"private_element")]);
            let length = long_length.len() - b"private_element".len() - 4;
            long_length[length..length + 4].copy_from_slice(&i32::MAX.to_be_bytes());
            long_length
        },
    ]);
    for bytes in malformed {
        refusal(Type::TEXT_ARRAY, bytes);
    }
    assert!(decode_value(&Type::TEXT_ARRAY, &valid).is_ok());
}

#[test]
fn non_text_arrays_remain_unsupported() {
    let integers = array(&[(1, 1)], 0, INT4_OID, &[Some(&7_i32.to_be_bytes())]);
    let message = refusal(Type::INT4_ARRAY, integers);
    assert!(message.contains("unsupported PostgreSQL type"), "{message}");
    assert!(decode_value(&Type::TEXT_ARRAY, &text_array(&[Some("seven")])).is_ok());
}
