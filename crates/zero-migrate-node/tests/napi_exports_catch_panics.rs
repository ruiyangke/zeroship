//! Every N-API export opts into `catch_unwind`, so no panic reaches the FFI edge.
//!
//! napi-rs wraps an export body in `catch_unwind` only when the export asks for it
//! (`napi-derive-backend/src/codegen/fn.rs`, the `if self.catch_unwind` branch).
//! Without it a panic unwinds out of the generated `extern "C"` shim, and Rust
//! turns that into an abort. Measured on `genArtifacts` before this was applied:
//!
//! ```text
//! fatal runtime error: failed to initiate panic, error 5, aborting
//! Aborted (core dumped)     node_rc=134
//! ```
//!
//! The process dies with no JS stack and nothing for a caller to catch. With the
//! attribute the same panic arrives as `Error: <panic message>` and the process
//! survives.
//!
//! This reads the source rather than provoking a panic, because the shipped code
//! has no panic to provoke on demand and adding one to reach a test would put the
//! hazard into the product to prove the product is safe from it. What the gate is
//! really for is COMPLETENESS: a reviewer reading one diff hunk cannot see that an
//! export elsewhere was added without the attribute.

/// The bridge source, read at compile time so this runs in the napi-free build
/// (the module itself is `#[cfg(feature = "napi")]`, and the gate must not be).
const BRIDGE: &str = include_str!("../src/bridge.rs");

/// Every export-level `#[napi(...)]` attribute, joined into one string each.
///
/// An export attribute starts at column zero; the ones indented inside a function
/// signature annotate an ARGUMENT and take no `catch_unwind`. `rustfmt` wraps a
/// long attribute over several lines, so the text is accumulated until parentheses
/// balance rather than read one line at a time - a single-line scan would see a
/// bare `#[napi(` and report a false failure, or worse, miss a real one.
fn export_attributes(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut lines = source.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        if !line.starts_with("#[napi(") {
            continue;
        }
        let mut attribute = line.to_string();
        let mut depth = balance(line);
        while depth > 0 {
            let Some((_, continuation)) = lines.next() else {
                break;
            };
            attribute.push_str(continuation.trim());
            depth += balance(continuation);
        }
        found.push((index + 1, attribute));
    }
    found
}

/// Open parens minus close parens on one line.
fn balance(line: &str) -> i32 {
    let opens = i32::try_from(line.matches('(').count()).expect("a line holds few parens");
    let closes = i32::try_from(line.matches(')').count()).expect("a line holds few parens");
    opens - closes
}

#[test]
fn every_napi_export_opts_into_catch_unwind() {
    let attributes = export_attributes(BRIDGE);

    // Pinned so a rename or a refactor that stops matching fails loudly instead of
    // passing over an empty set. Raise it deliberately when an export is added.
    assert!(
        attributes.len() >= 14,
        "matched only {} export attributes, so the shape this gate keys on has \
         changed and it is no longer checking anything",
        attributes.len()
    );

    let unguarded: Vec<String> = attributes
        .iter()
        .filter(|(_, attribute)| !attribute.contains("catch_unwind"))
        .map(|(number, attribute)| format!("bridge.rs:{number}: {attribute}"))
        .collect();

    assert!(
        unguarded.is_empty(),
        "these N-API exports would abort the Node process on a panic instead of \
         throwing; add `catch_unwind` to each:\n{}",
        unguarded.join("\n")
    );
}

#[test]
fn the_scanner_joins_an_attribute_rustfmt_wrapped_over_several_lines() {
    // Three of the real exports are wrapped, so this is not hypothetical. A scanner
    // that read one line at a time would see a bare `#[napi(` for each of them and
    // report them all as unguarded. Prove the joining works on the shape rustfmt
    // actually produces rather than trusting the main gate's silence.
    let wrapped = "#[napi(\n    js_name = \"applyIrSqlite\",\n    \
                   ts_return_type = \"Promise<ApplyReply>\",\n    catch_unwind\n)]\n";
    let joined = export_attributes(wrapped);
    assert_eq!(
        joined.len(),
        1,
        "the wrapped attribute is one unit: {joined:?}"
    );
    assert!(
        joined[0].1.contains("catch_unwind"),
        "the join has to carry the flag off the continuation line: {}",
        joined[0].1
    );

    // And the same shape WITHOUT the flag must still be caught.
    let unguarded = "#[napi(\n    js_name = \"applyIrSqlite\",\n    \
                     ts_return_type = \"Promise<ApplyReply>\"\n)]\n";
    let joined = export_attributes(unguarded);
    assert_eq!(joined.len(), 1);
    assert!(
        !joined[0].1.contains("catch_unwind"),
        "an unguarded wrapped attribute must not read as guarded: {}",
        joined[0].1
    );
}

/// The gate above reads ONE file, so it is only complete while every export lives
/// in it. That assumption is exactly the kind the gate exists to defend - its own
/// header says a reviewer "cannot see that an export elsewhere was added without
/// the attribute", and an export added to `verbs.rs` or `api.rs` would be invisible
/// to a scan of `bridge.rs`. So pin the assumption instead of trusting it.
///
/// This reads the source directory at test time rather than with `include_str!`,
/// because the hazard is a file that does not exist yet and so cannot be named in
/// an `include_str!` list - a list that must be updated to catch a new file catches
/// nothing.
#[test]
fn bridge_rs_is_the_only_file_that_exports_to_n_api() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut strays: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(&src).expect("the crate has a src directory") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .expect("a source file has a name")
            .to_string();
        if name == "bridge.rs" {
            continue;
        }

        let source = std::fs::read_to_string(&path).expect("a readable source file");
        // Only CALLABLE exports matter. `#[napi(object)]` on a struct crosses the
        // boundary as data and has no body to unwind out of, so it neither needs
        // nor accepts `catch_unwind`.
        for (number, attribute) in export_attributes(&source) {
            let declares_fn = source
                .lines()
                .skip(number) // `number` is 1-based, so this starts after the attribute
                .find(|line| {
                    let trimmed = line.trim_start();
                    !trimmed.is_empty() && !trimmed.starts_with("#[") && !trimmed.starts_with("//")
                })
                .is_some_and(|line| line.contains("fn "));
            if declares_fn {
                strays.push(format!("{name}:{number}: {attribute}"));
            }
        }
    }

    assert!(
        strays.is_empty(),
        "these N-API function exports live outside `bridge.rs`, where the \
         `catch_unwind` gate above cannot see them. Either move them into \
         `bridge.rs` or widen that gate to scan every source file:\n{}",
        strays.join("\n")
    );
}
