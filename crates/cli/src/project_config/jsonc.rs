//! JSONC parsing and original-source ranges backed by `jsonc-parser`.
//!
//! The parser is deliberately configured for JSON plus comments and trailing
//! commas only. Its defaults also accept several unrelated JavaScript-like
//! extensions, so every option is stated explicitly below.

use jsonc_parser::ast::Value as AstValue;
use jsonc_parser::common::Ranged;
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
    reject_extended_whitespace(text)?;
    parse_to_serde_value(text, &PARSE_OPTIONS).map_err(|error| error.to_string())
}

/// Return the original UTF-8 byte span of a top-level member's value.
pub fn top_level_value_span(text: &str, key: &str) -> Option<(usize, usize)> {
    reject_extended_whitespace(text).ok()?;
    let parsed = parse_to_ast(text, &CollectOptions::default(), &PARSE_OPTIONS).ok()?;
    let AstValue::Object(root) = parsed.value? else {
        return None;
    };
    let range = root.get(key)?.value.range();
    Some((range.start, range.end))
}

/// `jsonc-parser` accepts JavaScript whitespace beyond JSON's four code
/// points. Walk tokens with its scanner and reject any such skipped gaps while
/// leaving the contents of comments and strings entirely to the library.
fn reject_extended_whitespace(text: &str) -> Result<(), String> {
    let mut scanner = Scanner::new(text, &SCANNER_OPTIONS);
    let mut previous_end = 0;
    loop {
        let token = scanner.scan().map_err(|error| error.to_string())?;
        reject_gap(text, previous_end, scanner.token_start())?;
        match token {
            Some(_) => previous_end = scanner.token_end(),
            None => return Ok(()),
        }
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
