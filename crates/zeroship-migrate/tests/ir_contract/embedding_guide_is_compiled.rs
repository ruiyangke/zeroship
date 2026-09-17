//! The embedding guide's Rust examples must stay COMPILED, not just present.
//!
//! `crates/zeroship-migrate/src/lib.rs` includes the guide under `#[cfg(doctest)]`,
//! so rustdoc compiles every ```rust block in it against the real crate.
//!
//! What it cannot defend on its own is its own coverage. A ```rust,ignore fence is
//! invisible to rustdoc, and marking a block `ignore` is exactly what someone does
//! when a doc example stops compiling and the deadline is close. Silence every block
//! that way and `cargo test --doc` still reports success, having compiled nothing.
//!
//! So this asserts a floor on COMPILED examples, including the PostgreSQL apply
//! walkthrough and the SQLite backend open - the two a reader is most likely to copy.
//!
//! The `ignore`d blocks are deliberate and are not defects: they are fragments that
//! reference identifiers the surrounding prose introduces (`session`, `mysql`,
//! `engine`), plus one trait-signature listing that names types it never defines.
//! Making those compile means hidden setup lines, which is worth doing but not done
//! here.

use std::path::Path;

/// Fences that rustdoc will COMPILE. `rust,ignore` is deliberately excluded - that
/// is the spelling this test exists to keep an eye on.
fn compiled_rust_fences(markdown: &str) -> usize {
    markdown
        .lines()
        .filter(|line| {
            let fence = line.trim_end();
            fence == "```rust" || fence == "```rust,no_run" || fence == "```rust,should_panic"
        })
        .count()
}

fn ignored_rust_fences(markdown: &str) -> usize {
    markdown
        .lines()
        .filter(|line| line.trim_end().starts_with("```rust,ignore"))
        .count()
}

#[test]
fn the_embedding_guide_keeps_at_least_two_compiled_examples() {
    let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/embedding.md");
    let markdown = std::fs::read_to_string(&guide)
        .unwrap_or_else(|error| panic!("read {}: {error}", guide.display()));

    let compiled = compiled_rust_fences(&markdown);
    let ignored = ignored_rust_fences(&markdown);

    assert!(
        compiled >= 2,
        "docs/embedding.md carries {compiled} compiled Rust example(s) and {ignored} ignored. \
         The doctest gate only checks the compiled ones, so dropping below two means the \
         guide's main examples stopped being verified - most likely because a failing one \
         was marked `rust,ignore` instead of fixed. Fix the example, or move the floor \
         deliberately and say why."
    );
}

#[test]
fn an_ignored_fence_is_spelled_the_way_this_test_looks_for() {
    // The floor above is only meaningful if `ignore` is really invisible to the
    // counter - otherwise an ignored block would count as compiled and the floor
    // would pass while checking nothing. Pin both directions on a literal sample
    // rather than trusting the predicate to be read correctly.
    let sample = "```rust\nlet a = 1;\n```\n```rust,ignore\nnot code\n```\n";
    assert_eq!(
        compiled_rust_fences(sample),
        1,
        "only the plain fence compiles"
    );
    assert_eq!(
        ignored_rust_fences(sample),
        1,
        "the ignore fence is counted as ignored"
    );
}
