//! JSONC parsing and original-source ranges backed by `jsonc-parser`.
//!
//! The parser is deliberately configured for JSON plus comments and trailing
//! commas only. Its defaults also accept several unrelated JavaScript-like
//! extensions, so every option is stated explicitly below.

use jsonc_parser::ast::Value as AstValue;
use jsonc_parser::common::Ranged;
use jsonc_parser::tokens::Token;
use jsonc_parser::{
    parse_to_ast, parse_to_serde_value, CollectOptions, ParseOptions, Scanner,
    ScannerOptions,
};
use serde_json::Value;

const PARSE_OPTIONS: ParseOptions = ParseOptions {
    allow_comments: true,
    allow_loose_object_property_names: false,
    allow_trailing_commas: true,
    allow_missing_commas: false,
    allow_single_quoted_strings: false,
    allow_hexadecimal_numbers: false,
    allow_unary_plus_numbers: false,
};

const SCANNER_OPTIONS: ScannerOptions = ScannerOptions {
    allow_single_quoted_strings: false,
    allow_hexadecimal_numbers: false,
    allow_unary_plus_numbers: false,
};

/// Parse JSONC into the same `serde_json::Value` used by the rest of the CLI.
pub fn parse(text: &str) -> Result<Value, String> {
    validate_scanned_source(text)?;
    parse_to_serde_value(text, &PARSE_OPTIONS).map_err(|error| error.to_string())
}

/// Return the original UTF-8 byte span of a top-level member's value.
pub fn top_level_value_span(text: &str, key: &str) -> Option<(usize, usize)> {
    validate_scanned_source(text).ok()?;
    let parsed = parse_to_ast(text, &CollectOptions::default(), &PARSE_OPTIONS).ok()?;
    let AstValue::Object(root) = parsed.value? else {
        return None;
    };
    let range = root.get(key)?.value.range();
    Some((range.start, range.end))
}

/// Close two strict-JSON gaps in `jsonc-parser`: JavaScript whitespace beyond
/// JSON's four code points, and raw C0 controls inside quoted strings. The
/// scanner keeps comments distinct, so controls in comments remain harmless.
fn validate_scanned_source(text: &str) -> Result<(), String> {
    let mut scanner = Scanner::new(text, &SCANNER_OPTIONS);
    let mut previous_end = 0;
    loop {
        let token = scanner.scan().map_err(|error| error.to_string())?;
        reject_gap(text, previous_end, scanner.token_start())?;
        match token {
            Some(Token::String(_)) => {
                reject_raw_string_controls(text, scanner.token_start(), scanner.token_end())?;
                previous_end = scanner.token_end();
            }
            Some(_) => previous_end = scanner.token_end(),
            None => return Ok(()),
        }
    }
}

fn reject_raw_string_controls(text: &str, start: usize, end: usize) -> Result<(), String> {
    let content_start = start + 1;
    let content_end = end - 1;
    let invalid = text.as_bytes()[content_start..content_end]
        .iter()
        .position(|byte| *byte < 0x20);
    if let Some(offset) = invalid {
        Err(format!(
            "invalid raw control character in JSON string at byte {}",
            content_start + offset
        ))
    } else {
        Ok(())
    }
}

fn reject_gap(text: &str, start: usize, end: usize) -> Result<(), String> {
    let invalid = text[start..end]
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t' | '\n' | '\r'));
    if let Some((offset, _)) = invalid {
        Err(format!(
            "invalid JSON whitespace at byte {}",
            start + offset
        ))
    } else {
        Ok(())
    }
}
