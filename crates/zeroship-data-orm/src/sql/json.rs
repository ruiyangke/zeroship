//! Exact JSON comparison for SQL engines without structural JSON equality.
use serde_json::value::RawValue;
use std::collections::BTreeMap;

const MAX_COMPARISON_DEPTH: usize = 128;

/// An invalid or unrepresentable JSON comparison operand. Contents are private.
#[derive(Debug)]
pub struct ComparisonError;

impl std::fmt::Display for ComparisonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid JSON comparison operand")
    }
}
impl std::error::Error for ComparisonError {}

/// Produce an equality key with unordered object keys and exact decimal numbers.
/// This is an internal SQL comparison representation, not a wire format.
///
/// # Errors
/// Refuses invalid JSON, excessive nesting, and unrepresentable exponents.
pub fn comparison_key(json: &str) -> Result<String, ComparisonError> {
    let value = serde_json::from_str::<&RawValue>(json).map_err(|_| ComparisonError)?;
    let mut output = String::with_capacity(json.len());
    append_key(value, &mut output, 0)?;
    Ok(output)
}

fn append_key(value: &RawValue, output: &mut String, depth: usize) -> Result<(), ComparisonError> {
    if depth > MAX_COMPARISON_DEPTH {
        return Err(ComparisonError);
    }
    let raw = value.get();
    match raw.as_bytes().first() {
        Some(b'{') => {
            let fields: BTreeMap<String, &RawValue> =
                serde_json::from_str(raw).map_err(|_| ComparisonError)?;
            output.push('{');
            for (key, value) in fields {
                output.push_str(&serde_json::to_string(&key).map_err(|_| ComparisonError)?);
                output.push(':');
                append_key(value, output, depth + 1)?;
                output.push(',');
            }
            output.push('}');
        }
        Some(b'[') => {
            let values: Vec<&RawValue> = serde_json::from_str(raw).map_err(|_| ComparisonError)?;
            output.push('[');
            for value in values {
                append_key(value, output, depth + 1)?;
                output.push(',');
            }
            output.push(']');
        }
        Some(b'"') => {
            let text: String = serde_json::from_str(raw).map_err(|_| ComparisonError)?;
            output.push_str(&serde_json::to_string(&text).map_err(|_| ComparisonError)?);
        }
        Some(b'-' | b'0'..=b'9') => append_number(raw, output)?,
        _ => output.push_str(raw),
    }
    Ok(())
}

fn append_number(raw: &str, output: &mut String) -> Result<(), ComparisonError> {
    let (mantissa, exponent) =
        raw.split_once(['e', 'E'])
            .map_or(Ok((raw, 0_i64)), |(mantissa, exponent)| {
                exponent
                    .parse::<i64>()
                    .map(|exponent| (mantissa, exponent))
                    .map_err(|_| ComparisonError)
            })?;
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits = mantissa.replace('.', "");
    let significant = digits.trim_start_matches('0').trim_end_matches('0');
    if significant.is_empty() {
        output.push('0');
        return Ok(());
    }
    let trailing = digits.len() - digits.trim_end_matches('0').len();
    let exponent = exponent
        .checked_sub(i64::try_from(fraction).map_err(|_| ComparisonError)?)
        .and_then(|exponent| exponent.checked_add(i64::try_from(trailing).ok()?))
        .ok_or(ComparisonError)?;
    if negative {
        output.push('-');
    }
    output.push_str(significant);
    output.push('e');
    output.push_str(&exponent.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_preserves_json_types_and_exact_numbers() {
        for (left, right) in [
            (" \n 1 \t", "1"),
            ("1", "1.00e0"),
            ("-0.00", "0"),
            ("100", "1e2"),
            ("18446744073709551615", "18446744073709551615.0"),
            ("0.000000000000000000123", "123e-21"),
            (r#"{"a":1,"b":[true,null]}"#, r#"{"b":[true,null],"a":1.0}"#),
            (r#"{"a":2,"a":1}"#, r#"{"a":1}"#),
            (r#""\u0061""#, r#""a""#),
        ] {
            assert_eq!(
                comparison_key(left).unwrap(),
                comparison_key(right).unwrap(),
                "{left} vs {right}"
            );
        }
        for (left, right) in [
            ("1", "true"),
            ("null", r#""null""#),
            ("1", r#""1""#),
            ("18446744073709551615", "18446744073709551614"),
            ("18446744073709551615", "18446744073709551616.0"),
            ("[1,2]", "[2,1]"),
            (r#"{"a":1}"#, r#"{"a":1,"b":2}"#),
        ] {
            assert_ne!(
                comparison_key(left).unwrap(),
                comparison_key(right).unwrap(),
                "{left} vs {right}"
            );
        }
    }

    #[test]
    fn invalid_json_never_appears_in_errors() {
        for value in ["private_invalid_json", "NaN", "1e9999999999999999999999"] {
            let error = comparison_key(value).unwrap_err();
            assert!(!error.to_string().contains(value));
        }
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_COMPARISON_DEPTH + 1),
            "]".repeat(MAX_COMPARISON_DEPTH + 1)
        );
        assert!(comparison_key(&nested).is_err());
    }
}
