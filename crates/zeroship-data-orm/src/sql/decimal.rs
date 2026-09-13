use super::{compiler::CompileError, statement::DecimalStorage};
use num_bigint::{BigInt, Sign};

pub(crate) const MAX_PRECISION: u16 = 1000;
const MAX_INPUT_DIGITS: usize = 4096;
const MAX_EXPONENT: i32 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExactDecimal {
    coefficient: BigInt,
    scale: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Arithmetic {
    Add,
    Subtract,
    Multiply,
}

pub(crate) fn storage(
    definition: &crate::schema::ColumnSchema,
) -> Result<Option<DecimalStorage>, CompileError> {
    if definition.logical_type != crate::schema::LogicalType::Number {
        return Ok(None);
    }
    let Some(precision) = definition.precision else {
        return Ok(None);
    };
    DecimalStorage::new(precision, definition.scale.unwrap_or(0)).map(Some)
}

pub(crate) fn valid(value: &str) -> bool {
    ExactDecimal::parse(value).is_ok()
}

pub(crate) fn equivalent(left: &str, right: &str) -> Result<bool, DecimalError> {
    Ok(ExactDecimal::parse(left)? == ExactDecimal::parse(right)?)
}

pub(crate) fn quantize(value: &str, storage: DecimalStorage) -> Result<String, DecimalError> {
    ExactDecimal::parse(value)?.quantize(storage)
}

pub(crate) fn arithmetic(
    left: &str,
    right: &str,
    operation: Arithmetic,
    storage: DecimalStorage,
) -> Result<String, DecimalError> {
    let left = ExactDecimal::parse(left)?;
    let right = ExactDecimal::parse(right)?;
    let value = match operation {
        Arithmetic::Add => left.add(right, false),
        Arithmetic::Subtract => left.add(right, true),
        Arithmetic::Multiply => ExactDecimal {
            coefficient: left.coefficient * right.coefficient,
            scale: left.scale + right.scale,
        },
    };
    value.quantize(storage)
}

impl ExactDecimal {
    fn parse(value: &str) -> Result<Self, DecimalError> {
        let bytes = value.as_bytes();
        if bytes.len() > MAX_INPUT_DIGITS {
            return Err(DecimalError);
        }
        let mut cursor = 0;
        let negative = bytes.first() == Some(&b'-');
        if negative {
            cursor += 1;
        }
        let integer_start = cursor;
        match bytes.get(cursor) {
            Some(b'0') => cursor += 1,
            Some(b'1'..=b'9') => {
                cursor += 1;
                while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                    cursor += 1;
                }
            }
            _ => return Err(DecimalError),
        }
        let integer_end = cursor;
        let mut fraction_start = cursor;
        if bytes.get(cursor) == Some(&b'.') {
            cursor += 1;
            fraction_start = cursor;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            if fraction_start == cursor {
                return Err(DecimalError);
            }
        }
        let fraction_end = cursor;
        let exponent = if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
            cursor += 1;
            let sign = match bytes.get(cursor) {
                Some(b'+') => {
                    cursor += 1;
                    1
                }
                Some(b'-') => {
                    cursor += 1;
                    -1
                }
                _ => 1,
            };
            let start = cursor;
            let mut exponent = 0i32;
            while let Some(digit @ b'0'..=b'9') = bytes.get(cursor) {
                exponent = exponent
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(i32::from(*digit - b'0')))
                    .filter(|value| *value <= MAX_EXPONENT)
                    .ok_or(DecimalError)?;
                cursor += 1;
            }
            if start == cursor {
                return Err(DecimalError);
            }
            exponent * sign
        } else {
            0
        };
        if cursor != bytes.len() {
            return Err(DecimalError);
        }

        let mut digits =
            String::with_capacity(integer_end - integer_start + fraction_end - fraction_start);
        digits.push_str(&value[integer_start..integer_end]);
        digits.push_str(&value[fraction_start..fraction_end]);
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Ok(Self {
                coefficient: BigInt::from(0),
                scale: 0,
            });
        }
        if digits.len() > MAX_INPUT_DIGITS {
            return Err(DecimalError);
        }
        let mut digits = digits.to_owned();
        let mut scale =
            i32::try_from(fraction_end - fraction_start).map_err(|_| DecimalError)? - exponent;
        if scale < 0 {
            let zeros = usize::try_from(-scale).map_err(|_| DecimalError)?;
            if digits
                .len()
                .checked_add(zeros)
                .is_none_or(|len| len > MAX_INPUT_DIGITS)
            {
                return Err(DecimalError);
            }
            digits.extend(std::iter::repeat_n('0', zeros));
            scale = 0;
        }
        let mut scale = usize::try_from(scale).map_err(|_| DecimalError)?;
        if scale > MAX_INPUT_DIGITS {
            return Err(DecimalError);
        }
        while scale > 0 && digits.ends_with('0') {
            digits.pop();
            scale -= 1;
        }
        let mut coefficient = BigInt::parse_bytes(digits.as_bytes(), 10).ok_or(DecimalError)?;
        if negative {
            coefficient = -coefficient;
        }
        Ok(Self { coefficient, scale })
    }

    fn add(self, other: Self, subtract: bool) -> Self {
        let scale = self.scale.max(other.scale);
        let left = self.coefficient * power_of_ten(scale - self.scale);
        let mut right = other.coefficient * power_of_ten(scale - other.scale);
        if subtract {
            right = -right;
        }
        Self {
            coefficient: left + right,
            scale,
        }
    }

    fn quantize(self, storage: DecimalStorage) -> Result<String, DecimalError> {
        let target_scale = usize::from(storage.scale());
        let mut coefficient = if self.scale < target_scale {
            self.coefficient * power_of_ten(target_scale - self.scale)
        } else if self.scale > target_scale {
            round(self.coefficient, self.scale - target_scale)
        } else {
            self.coefficient
        };
        if coefficient == BigInt::from(0) {
            coefficient = BigInt::from(0);
        }
        let negative = coefficient.sign() == Sign::Minus;
        let mut digits = if negative {
            (-&coefficient).to_string()
        } else {
            coefficient.to_string()
        };
        if digits.len() > usize::from(storage.precision()) {
            return Err(DecimalError);
        }
        if target_scale > 0 {
            if digits.len() <= target_scale {
                digits.insert_str(0, &"0".repeat(target_scale + 1 - digits.len()));
            }
            digits.insert(digits.len() - target_scale, '.');
        }
        if negative {
            digits.insert(0, '-');
        }
        Ok(digits)
    }
}

fn round(coefficient: BigInt, places: usize) -> BigInt {
    if coefficient == BigInt::from(0) {
        return coefficient;
    }
    let divisor = power_of_ten(places);
    let quotient = &coefficient / &divisor;
    let remainder = &coefficient % &divisor;
    let magnitude = if remainder.sign() == Sign::Minus {
        -remainder
    } else {
        remainder
    };
    if magnitude * 2 >= divisor {
        quotient
            + if coefficient.sign() == Sign::Minus {
                BigInt::from(-1)
            } else {
                BigInt::from(1)
            }
    } else {
        quotient
    }
}

fn power_of_ten(power: usize) -> BigInt {
    BigInt::from(10u8).pow(u32::try_from(power).expect("bounded decimal exponent"))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DecimalError;

impl std::fmt::Display for DecimalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid or out-of-range exact decimal")
    }
}

impl std::error::Error for DecimalError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_math_normalizes_spelling_and_rounds_to_storage_scale() {
        assert!(equivalent("1.0", "1.00").unwrap());
        assert!(equivalent("123e-2", "1.23").unwrap());
        let storage = DecimalStorage::new(8, 2).unwrap();
        assert_eq!(quantize("1.005", storage).unwrap(), "1.01");
        assert_eq!(quantize("-1.005", storage).unwrap(), "-1.01");
        assert_eq!(
            arithmetic(
                "9007199254740993.00",
                "0.01",
                Arithmetic::Add,
                DecimalStorage::new(30, 2).unwrap()
            )
            .unwrap(),
            "9007199254740993.01"
        );
    }

    #[test]
    fn exact_math_refuses_invalid_and_overflowing_values() {
        for value in ["", "+1", "01", "1.", ".1", "NaN", "1e99999"] {
            assert!(!valid(value), "{value}");
        }
        assert!(!valid(&format!("1e{}1", "0".repeat(MAX_INPUT_DIGITS))));
        assert!(quantize("999.5", DecimalStorage::new(3, 0).unwrap()).is_err());
    }
}
