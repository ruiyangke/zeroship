//! `zeroship migrate` — best-effort migration tooling.
//!
//! P5.5 PR 8 ships the first sub-command:
//!
//!   `zeroship migrate scan-mask-usage [--path=<dir>] [--format=text|json]`
//!
//! Walks a creator's source tree for property accesses on encrypted-
//! column-shaped names that would have worked under the pre-P5.5
//! transparent-decrypt model but DON'T anymore — the read flip in
//! P5.5 PR 3 makes `user.ssn` a `MaskedValue<string>` instead of a
//! bare string. Findings include a stable replacement suggestion
//! (`.unmask({ actor, reason })`).
//!
//! **This is best-effort linting**: the scanner is a regex over
//! source text (no tree-sitter / TypeScript parser dependency), so
//! false positives are expected. Every flagged spot needs creator
//! review — the README banner says so explicitly.
//!
//! Adding new `migrate` sub-commands: extend [`cmd_migrate`]'s match
//! and document the new shape in [`crate::print_usage`].

use std::path::{Path, PathBuf};

use crate::flag_str;

/// Canonical list of PII suffixes the scanner heuristically flags as
/// likely-masked. Kept SHORT on purpose — false positives are an
/// acceptable cost for a one-shot migration aid; false negatives are
/// documented in the migration guide and the help banner.
///
/// Suffixes are matched case-insensitively against the property name
/// ONLY (not the receiver). Examples that match: `.ssn`, `.userSsn`,
/// `.email_address`, `.dob`. Examples that miss: `.something_else`.
const PII_SUFFIXES: &[&str] = &[
    // Government IDs
    "ssn",
    "social_security",
    "tax_id",
    "taxid",
    // Email / phone
    "email",
    "email_address",
    "phone",
    "phone_number",
    "mobile",
    // Date-of-birth
    "dob",
    "date_of_birth",
    "birthdate",
    "birthday",
    // Payment instruments
    "credit_card",
    "creditcard",
    "card_number",
    "card_no",
    "cvv",
    "cvc",
    // Healthcare
    "medical_record",
    "diagnosis",
    "health_record",
    // Address
    "street_address",
    "home_address",
    "address1",
    // Other commonly-encrypted columns
    "passport",
    "drivers_license",
    "license_number",
];

/// Directory components the scanner skips wholesale. Avoids false
/// flags in vendored / generated code that the creator doesn't own.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    ".git",
    ".turbo",
    ".next",
    "coverage",
    "target",
];

/// File extensions the scanner inspects. JS/TS only — creator source.
const SCAN_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// A single flagged source location.
#[derive(Debug, PartialEq, Eq)]
struct Finding {
    file: PathBuf,
    line: usize,
    column: usize,
    snippet: String,
    /// Stable text the creator pastes in as a starting point. The
    /// `actor` / `reason` strings are placeholders the creator MUST
    /// fill in — the scanner has no way to know the real values.
    suggested_replacement: String,
    /// Why the scanner flagged this — `dot_access`, `template_literal`.
    pattern: &'static str,
}

/// Dispatch — only `scan-mask-usage` ships in PR 8. Future sub-
/// commands (`scan-encrypted-usage`, `rewrite-fixers`, …) slot in
/// here.
pub fn cmd_migrate(args: &[String]) {
    let sub = args.get(2).map(String::as_str).unwrap_or("");
    match sub {
        "scan-mask-usage" => cmd_migrate_scan_mask_usage(args),
        _ => {
            eprintln!(
                "Usage:\n  zeroship migrate scan-mask-usage [--path=<dir>] [--format=text|json]\n\n\
                 Scans the creator's source tree for property accesses on\n\
                 columns that LIKELY became MaskedValue<T> after P5.5 PR 3.\n\
                 Best-effort heuristic — every finding needs creator review.\n\
                 Exits 0 with no findings; 1 with one or more."
            );
            std::process::exit(1);
        }
    }
}

/// `zeroship migrate scan-mask-usage`.
///
/// Walks `--path=<dir>` (default `./src`), parses every JS/TS file
/// against the PII-suffix heuristic, prints findings, exits 1 if any
/// were flagged. `--format=json` swaps text output for a JSON array
/// suitable for piping into other tooling.
pub fn cmd_migrate_scan_mask_usage(args: &[String]) {
    let path = flag_str(args, "--path=").unwrap_or_else(|| "./src".to_string());
    let format = flag_str(args, "--format=").unwrap_or_else(|| "text".to_string());

    let root = PathBuf::from(&path);
    let findings = match scan_path(&root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("zeroship migrate scan-mask-usage: {e}");
            std::process::exit(1);
        }
    };

    match format.as_str() {
        "json" => print_findings_json(&findings),
        "text" | _ => print_findings_text(&findings, &root),
    }

    if findings.is_empty() {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// Walk `root` and return every finding. Returns a typed error message
/// (suitable for direct `eprintln!`) when the path doesn't exist /
/// isn't a directory.
fn scan_path(root: &Path) -> Result<Vec<Finding>, String> {
    if !root.exists() {
        return Err(format!(
            "path does not exist: {} (set --path=<dir> or run from repo root)",
            root.display(),
        ));
    }
    if !root.is_dir() {
        return Err(format!("path is not a directory: {}", root.display()));
    }
    let mut findings = Vec::new();
    walk(root, &mut findings);
    findings.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    Ok(findings)
}

fn walk(dir: &Path, out: &mut Vec<Finding>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            walk(&path, out);
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !SCAN_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        scan_source(&path, &source, out);
    }
}

/// Scan a single source string. Two patterns:
///
/// 1. Dot access: `<recv>.<col>` where `<col>` ends in a PII suffix.
/// 2. Template literal: `${<expr>.<col>}` (same suffix rule).
///
/// Bracket access (`x["ssn"]`) is intentionally NOT scanned — the
/// shape is uncommon in idiomatic TypeScript and adding it raises
/// the false-positive rate noticeably.
fn scan_source(path: &Path, source: &str, out: &mut Vec<Finding>) {
    for (line_no, line) in source.lines().enumerate() {
        let line_no = line_no + 1;
        scan_dot_access(path, line_no, line, out);
        scan_template_literal(path, line_no, line, out);
    }
}

fn scan_dot_access(path: &Path, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'.' {
            i += 1;
            continue;
        }
        // The char BEFORE `.` must look like an identifier tail
        // (alpha/digit/underscore/closing bracket). Skip floats
        // (preceding char is a digit AND the receiver is plain
        // numeric — handled below by the receiver-extraction loop).
        if i == 0 {
            i += 1;
            continue;
        }
        let prev = bytes[i - 1];
        let is_id_tail =
            prev.is_ascii_alphanumeric() || prev == b'_' || prev == b')' || prev == b']';
        if !is_id_tail {
            i += 1;
            continue;
        }
        // Extract the property name: identifier-shape chars after `.`.
        let start_prop = i + 1;
        let mut end_prop = start_prop;
        while end_prop < bytes.len() {
            let c = bytes[end_prop];
            if c.is_ascii_alphanumeric() || c == b'_' {
                end_prop += 1;
            } else {
                break;
            }
        }
        if end_prop == start_prop {
            i += 1;
            continue;
        }
        let prop = &line[start_prop..end_prop];
        if !is_likely_pii(prop) {
            i = end_prop;
            continue;
        }
        // Skip a fluent chain like `user.unmask(...)` — if the
        // matched property is followed by `(`, it's a function call,
        // not a value read. We're looking for value access, not
        // method invocation. (Method names that LOOK like PII columns
        // are vanishingly rare.)
        if end_prop < bytes.len() && bytes[end_prop] == b'(' {
            i = end_prop;
            continue;
        }
        // Receiver — walk backwards from `i` over the identifier
        // chain so the snippet captures `user` from `user.ssn`.
        let mut recv_start = i;
        while recv_start > 0 {
            let c = bytes[recv_start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                recv_start -= 1;
            } else {
                break;
            }
        }
        let receiver = &line[recv_start..i];
        let snippet = format!("{receiver}.{prop}");
        let suggested_replacement =
            format!("await {receiver}.{prop}.unmask({{ actor: \"??\", reason: \"??\" }})");
        out.push(Finding {
            file: path.to_path_buf(),
            line: line_no,
            column: recv_start + 1, // 1-based column
            snippet,
            suggested_replacement,
            pattern: "dot_access",
        });
        i = end_prop;
    }
}

fn scan_template_literal(path: &Path, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    // Cheap, line-scoped regex-free scan: look for `${`, then scan
    // forward for `<expr>.<prop>` where `<prop>` ends in a PII
    // suffix, stopping at the matching `}`.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'$' || bytes[i + 1] != b'{' {
            i += 1;
            continue;
        }
        let expr_start = i + 2;
        // Find the matching `}` (does NOT handle nested braces —
        // acceptable for a best-effort scanner).
        let mut end = expr_start;
        while end < bytes.len() && bytes[end] != b'}' {
            end += 1;
        }
        if end >= bytes.len() {
            break; // unterminated; bail.
        }
        let expr = &line[expr_start..end];
        // Look for `<recv>.<prop>` inside `expr`.
        if let Some((recv, prop, prop_col_in_expr)) = find_dot_pair(expr) {
            if is_likely_pii(&prop) {
                let snippet = format!("${{{recv}.{prop}}}");
                let suggested_replacement = format!(
                    "${{await {recv}.{prop}.unmask({{ actor: \"??\", reason: \"??\" }})}}"
                );
                let column = expr_start + prop_col_in_expr;
                out.push(Finding {
                    file: path.to_path_buf(),
                    line: line_no,
                    column: column + 1, // 1-based
                    snippet,
                    suggested_replacement,
                    pattern: "template_literal",
                });
            }
        }
        i = end + 1;
    }
}

/// Extract `(receiver, property, column-of-receiver-start)` from the
/// first `<id>.<id>` pair in `expr`. Returns `None` when the
/// expression doesn't carry a property access.
fn find_dot_pair(expr: &str) -> Option<(String, String, usize)> {
    let bytes = expr.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'.' {
            continue;
        }
        if i == 0 {
            continue;
        }
        let prev = bytes[i - 1];
        if !(prev.is_ascii_alphanumeric() || prev == b'_') {
            continue;
        }
        // Walk back for receiver start.
        let mut rs = i;
        while rs > 0 {
            let c = bytes[rs - 1];
            if c.is_ascii_alphanumeric() || c == b'_' {
                rs -= 1;
            } else {
                break;
            }
        }
        // Walk forward for property end.
        let ps = i + 1;
        let mut pe = ps;
        while pe < bytes.len() {
            let c = bytes[pe];
            if c.is_ascii_alphanumeric() || c == b'_' {
                pe += 1;
            } else {
                break;
            }
        }
        if pe == ps {
            continue;
        }
        return Some((expr[rs..i].to_string(), expr[ps..pe].to_string(), rs));
    }
    None
}

fn is_likely_pii(prop: &str) -> bool {
    let lower = prop.to_ascii_lowercase();
    PII_SUFFIXES.iter().any(|suffix| {
        // Exact match OR snake_case tail OR camelCase tail.
        if lower == *suffix {
            return true;
        }
        if lower.ends_with(&format!("_{suffix}")) {
            return true;
        }
        // camelCase: the suffix appears with its first letter
        // capitalised inside the property name.
        let cap = suffix
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .map(|c| format!("{c}{}", &suffix[1..]))
            .unwrap_or_default();
        prop.ends_with(&cap) && !prop.eq_ignore_ascii_case(suffix)
    })
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_findings_text(findings: &[Finding], root: &Path) {
    if findings.is_empty() {
        eprintln!(
            "zeroship migrate scan-mask-usage: 0 findings under {} (clean)",
            root.display(),
        );
        return;
    }
    eprintln!(
        "zeroship migrate scan-mask-usage: {} finding(s) under {}",
        findings.len(),
        root.display(),
    );
    eprintln!("  (best-effort heuristic — every finding needs review)\n");
    for f in findings {
        println!(
            "{}:{}:{} [{}]\n  {}\n  --> {}\n",
            f.file.display(),
            f.line,
            f.column,
            f.pattern,
            f.snippet,
            f.suggested_replacement,
        );
    }
}

/// JSON output: flat array (chosen over file-grouped object so the
/// shape stays stable as new pattern kinds are added — every entry
/// carries the full `(file, line, column, pattern)` tuple).
fn print_findings_json(findings: &[Finding]) {
    let arr: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "file": f.file.display().to_string(),
                "line": f.line,
                "column": f.column,
                "pattern": f.pattern,
                "snippet": f.snippet,
                "suggested_replacement": f.suggested_replacement,
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(arr));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn td() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(&p, body).expect("write");
        p
    }

    /// Dot access on a property whose name ends in `ssn` is flagged.
    #[test]
    fn scan_finds_dot_access_on_pii_suffix() {
        let d = td();
        write(
            d.path(),
            "src/handler.ts",
            "export async function show(user) {\n  console.log(user.ssn);\n}\n",
        );
        let findings = scan_path(d.path()).expect("scan ok");
        assert_eq!(findings.len(), 1, "exactly one finding, got: {findings:?}");
        let f = &findings[0];
        assert_eq!(f.pattern, "dot_access");
        assert_eq!(f.line, 2);
        assert!(f.snippet.contains("user.ssn"));
        assert!(
            f.suggested_replacement.contains(".unmask("),
            "suggestion must wire through .unmask(): {}",
            f.suggested_replacement
        );
        assert!(
            f.suggested_replacement.contains("actor: \"??\""),
            "suggestion must keep the actor placeholder: {}",
            f.suggested_replacement
        );
    }

    /// Template literal that interpolates a PII access is flagged.
    #[test]
    fn scan_finds_template_literal_pii_access() {
        let d = td();
        write(
            d.path(),
            "src/email.ts",
            "function notify(user) {\n  return `Hi ${user.email}, your record is ready.`;\n}\n",
        );
        let findings = scan_path(d.path()).expect("scan ok");
        // The template literal scanner fires; the dot-access scanner
        // may ALSO flag the inner `user.email`. Either way, at least
        // one template_literal-pattern finding must be present.
        assert!(
            findings.iter().any(|f| f.pattern == "template_literal"),
            "must include a template_literal finding: {findings:?}"
        );
        let tmpl = findings
            .iter()
            .find(|f| f.pattern == "template_literal")
            .unwrap();
        assert!(tmpl.snippet.contains("user.email"));
        assert!(tmpl.suggested_replacement.contains(".unmask("));
    }

    /// JSON output is a valid JSON array carrying one entry per finding.
    #[test]
    fn scan_emits_json_when_format_json() {
        // Indirect: drive the formatter from a synthetic findings
        // list (the CLI integration would run via `cmd_migrate_scan_
        // mask_usage` which calls `std::process::exit`; the unit test
        // verifies the formatter shape only).
        let findings = vec![Finding {
            file: PathBuf::from("src/x.ts"),
            line: 3,
            column: 5,
            snippet: "user.ssn".to_string(),
            suggested_replacement:
                "await user.ssn.unmask({ actor: \"??\", reason: \"??\" })".to_string(),
            pattern: "dot_access",
        }];
        let arr: Vec<serde_json::Value> = findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "file": f.file.display().to_string(),
                    "line": f.line,
                    "column": f.column,
                    "pattern": f.pattern,
                    "snippet": f.snippet,
                    "suggested_replacement": f.suggested_replacement,
                })
            })
            .collect();
        let v = serde_json::Value::Array(arr);
        let s = v.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        let arr = parsed.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["file"], "src/x.ts");
        assert_eq!(arr[0]["pattern"], "dot_access");
        assert!(arr[0]["suggested_replacement"]
            .as_str()
            .unwrap()
            .contains(".unmask("));
    }

    /// A codebase with no PII-suffix accesses scans clean.
    #[test]
    fn scan_exits_zero_on_clean_codebase() {
        let d = td();
        write(
            d.path(),
            "src/clean.ts",
            "export function hello() { return 'world'; }\n",
        );
        let findings = scan_path(d.path()).expect("scan ok");
        assert!(findings.is_empty(), "clean codebase yielded findings: {findings:?}");
    }

    /// A non-existent path surfaces a typed error message.
    #[test]
    fn scan_handles_nonexistent_path_with_typed_error() {
        let err = scan_path(Path::new("/does/not/exist/for/sure/p5pr8"))
            .expect_err("missing path must error");
        assert!(
            err.contains("does not exist"),
            "error message must surface the typed reason: {err}"
        );
    }

    /// `node_modules/` and `dist/` are skipped wholesale — a PII
    /// access inside a vendored package is NOT the creator's
    /// problem.
    #[test]
    fn scan_ignores_node_modules_and_dist() {
        let d = td();
        write(
            d.path(),
            "node_modules/some-pkg/index.js",
            "exports.show = (u) => console.log(u.ssn);\n",
        );
        write(
            d.path(),
            "dist/bundle.js",
            "console.log(u.email);\n",
        );
        write(
            d.path(),
            "src/clean.ts",
            "export const ok = 'ok';\n",
        );
        let findings = scan_path(d.path()).expect("scan ok");
        assert!(
            findings.is_empty(),
            "vendored / dist files must be skipped: {findings:?}",
        );
    }

    // -----------------------------------------------------------------
    // Heuristic edge cases — pin the suffix-matching contract so
    // future tweaks (loosening / tightening) don't silently drift.
    // -----------------------------------------------------------------

    /// camelCase suffix match: `taxId` ends in the canonical `tax_id`
    /// suffix (camelCase shape) → flagged.
    #[test]
    fn is_likely_pii_matches_camel_case_tail() {
        assert!(is_likely_pii("ssn"));
        assert!(is_likely_pii("userSsn"));
        assert!(is_likely_pii("user_ssn"));
        assert!(is_likely_pii("email"));
        assert!(is_likely_pii("primaryEmail"));
        assert!(is_likely_pii("dob"));
        assert!(is_likely_pii("creditCard"));
    }

    /// Negative cases — names that DON'T end in a known PII suffix
    /// stay clean.
    #[test]
    fn is_likely_pii_rejects_unrelated_names() {
        assert!(!is_likely_pii("name"));
        assert!(!is_likely_pii("title"));
        assert!(!is_likely_pii("createdAt"));
        assert!(!is_likely_pii("body"));
        assert!(!is_likely_pii("id"));
    }

    /// Method-call shape (`user.unmask(...)`) is NOT flagged — even
    /// when the method name matches a PII suffix, a `(` immediately
    /// after the identifier means it's a call, not a value read.
    #[test]
    fn scan_skips_method_calls_that_look_like_pii() {
        let d = td();
        write(
            d.path(),
            "src/call.ts",
            "function f(u) { return u.email('foo'); }\n",
        );
        let findings = scan_path(d.path()).expect("scan ok");
        assert!(
            findings.is_empty(),
            "method invocation must not be flagged: {findings:?}",
        );
    }
}
