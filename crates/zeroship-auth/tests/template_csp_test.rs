use std::path::PathBuf;

#[test]
fn auth_templates_do_not_use_inline_style_attributes() {
    // Auth responses enforce `style-src 'self'`, so browsers reject inline
    // style attributes. This source guard scans Askama templates only; it does
    // not catch inline styles injected by Rust handler code at runtime.
    let templates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("ui")
        .join("templates");
    let mut paths = std::fs::read_dir(&templates)
        .unwrap_or_else(|error| panic!("read {}: {error}", templates.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("read entry in {}: {error}", templates.display()))
                .path()
        })
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();

    assert!(!paths.is_empty(), "no auth templates were scanned");
    for path in paths {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            !source.to_ascii_lowercase().contains("style=\""),
            "{} contains an inline style attribute blocked by the auth CSP",
            path.display()
        );
    }
}
