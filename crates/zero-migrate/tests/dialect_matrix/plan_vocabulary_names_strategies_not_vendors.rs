//! Core's lowered-plan vocabulary names STRATEGIES; only a genuinely vendor-shaped
//! type may name a vendor.
//!
//! `docs/proposals/pluggable-backends.md` wants the three vendors extracted into their
//! own crates behind a registered facade. A `pub` type in `render/plan.rs` or a
//! `RenameStep` variant in `render/step.rs` is the SHARED vocabulary every backend
//! speaks — it is the part of core that survives the extraction and that a
//! fourth-vendor crate would have to implement against. A vendor name in that
//! vocabulary is the coupling the whole refactor exists to remove: it tells a new
//! backend author that the concept belongs to someone else, and it tells a reader a
//! guarantee the code does not provide.
//!
//! # The name that was factually wrong, which is what makes this a test and not taste
//!
//! `RenameStep::PgExpandContract` was constructed for **MySQL**. Measured at the
//! commit before this file existed:
//!
//! * `render/declarative.rs` authors the expand-contract rename with no dialect
//!   guard. The only gate in that span is `if is_sqlite`, which `continue`s past the
//!   author entirely; PostgreSQL and MySQL both fall through to `renames.push(plan)`.
//! * `engine.rs`'s `apply_declarative_locked` maps EVERY `plan.renames` entry to the
//!   variant unconditionally. The only dialect mentioned in that whole function is a
//!   COMMENT claiming "Empty `rebuilds` on PG and empty `renames` on SQLite" — a
//!   two-dialect claim in a three-dialect codebase, silent on MySQL.
//! * Through the real public differ: Postgres `renames` = 1, SQLite `renames` = 0,
//!   **MySQL `renames` = 1**.
//!
//! So the variant's doc line ("**Postgres**: an online expand-contract", "chosen by
//! the deploy-target dialect at lowering") documented a one-vendor guarantee against
//! a two-vendor construction site. Renaming it to the strategy it actually is —
//! expand-contract, as against a table rebuild — makes the type honest. Whether the
//! APPLY path is correct for a MySQL expand-contract is a SEPARATE question this file
//! does not answer and this rename did not change.
//!
//! # Why a source scan rather than a behaviour test
//!
//! A rename is byte-identical at runtime by construction: same variants, same
//! payloads, same order, same SQL. Every other test in the workspace is indifferent
//! to which spelling this vocabulary is in — 37 suites / 3396 passed / 0 failed / 11
//! ignored against live PostgreSQL and MySQL with `ZERO_MIGRATE_REQUIRE_LIVE_DB=1`,
//! identical before and after. That indifference cuts both ways: nothing stops the
//! vendor spelling coming back, and nothing would notice. So the rule gets its own
//! check or it has none.
//!
//! # What is NOT a violation
//!
//! A type that really is one vendor's SHOULD say so; renaming that would be dishonest
//! in the other direction. [`SqliteSequencePolicy`] models the `sqlite_sequence` table
//! and the `AUTOINCREMENT` high-water mark — artifacts no other engine has — so it is
//! exempted BY NAME below, with that reason attached. The exemption list is
//! deliberately one entry long: a second one should have to be argued for in a diff.
//!
//! The surrounding rebuild vocabulary is not vendor-shaped and is not exempt. Of the
//! nine fields on the rebuild spec, eight (`table`, `tmp_table`, `new_table_create`,
//! `copy_columns`, `recreate_objects`, `column_renames`, `dropped_columns`, `reason`)
//! are ordinary create-copy-swap vocabulary that any engine without a native `ALTER`
//! would need; exactly one, `sequence_policy`, is SQLite's. The "12-step procedure"
//! appears only in doc comments, never in the field structure. That measured 8-of-9
//! split is why the shell is neutral and the one field keeps its vendor name.
//!
//! # What this does NOT catch
//!
//! It reads `render/step.rs` and `render/plan.rs` only, and within those, only
//! `RenameStep`'s variants and column-zero `pub` type declarations. It is blind to
//! vendor names on FIELDS, on private types, on types declared elsewhere that are not
//! reachable as a `RenameStep` payload, and to `apply/` — where `PostgresBackend`,
//! `MysqlBackend` and `SqliteBackend` are correctly vendor-named because they ARE the
//! vendor's backend.

/// The shared plan vocabulary, scanned for vendor spellings.
///
/// # How the regions are cut, and why not by brace matching
///
/// A naive brace counter would be wrong on these files: the surrounding modules are
/// full of `format!` literals containing braces, and a counter that cannot tell a
/// literal from code walks off the end of the item it meant to read. The cut here is
/// rustfmt's own invariant instead — a top-level item's closing brace is the line `}`
/// at column zero, and nothing nested can produce one — so a region runs from a
/// column-zero declaration to the next bare `}` line.
///
/// That cut also excludes the doc comments, which is load-bearing in the same way it
/// is for `schema_emitters_do_not_relookup_a_backend`: the prose here discusses
/// PostgreSQL, MySQL and SQLite by name constantly, and it must, because explaining
/// which engine produces which arm is exactly what those comments are for. A scan
/// that read prose would be red for a paragraph, which is the classic way a
/// source-scanning check gets weakened until it means nothing.
///
/// # The census clause is the boundary self-check, not decoration
///
/// Both scans fail OPEN. If a needle broke — `pub enum RenameStep {` respelled, the
/// region cutter defeated by a rustfmt change — the loop would iterate over nothing
/// and report green, which is the failure direction that matters. So the count of
/// names actually examined is asserted too, as a FLOOR: growth passes silently, a
/// broken scanner and a deleted concept both go red and have to be answered out loud.
#[test]
fn the_lowered_plan_vocabulary_names_no_vendor() {
    const STEP_SRC: &str = include_str!("../../src/render/step.rs");
    const PLAN_SRC: &str = include_str!("../../src/render/plan.rs");

    /// Vendor spellings as they appear inside a CamelCase identifier. `Pg` is listed
    /// separately from `Postgres` because both spellings are live in this crate.
    const VENDORS: [&str; 4] = ["Pg", "Postgres", "Mysql", "Sqlite"];

    /// The one type whose SHAPE is a vendor's, so its NAME should be too: it models
    /// the `sqlite_sequence` row and the `AUTOINCREMENT` high-water mark, which no
    /// other engine has. Adding an entry here is a claim that a second type is
    /// genuinely one vendor's — make it in a diff, with the artifact named.
    const VENDOR_SHAPED: [&str; 1] = ["SqliteSequencePolicy"];

    /// Every CamelCase identifier on a line — a variant name and its payload types
    /// both, so `Foo(BarPlan)` yields `Foo` and `BarPlan`.
    fn camel_idents(line: &str) -> Vec<&str> {
        line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|t| t.starts_with(|c: char| c.is_ascii_uppercase()))
            .collect()
    }

    /// The lines of `src`'s top-level item introduced by `header`, exclusive of the
    /// header line and of the closing column-zero `}`.
    fn item_body<'a>(src: &'a str, header: &str) -> Vec<&'a str> {
        let lines: Vec<&str> = src.lines().collect();
        let Some(start) = lines.iter().position(|l| *l == header) else {
            return Vec::new();
        };
        let end = lines[start..]
            .iter()
            .position(|l| *l == "}")
            .map_or(lines.len(), |offset| start + offset);
        lines[start + 1..end].to_vec()
    }

    let mut scanned: Vec<(&str, String)> = Vec::new();

    // (a) The `RenameStep` arms: the variant names AND their payload types. Both are
    // vocabulary — a neutral variant wrapping `SqliteRebuild` has moved the vendor
    // name one line down, not removed it.
    let mut arms = 0usize;
    for line in item_body(STEP_SRC, "pub enum RenameStep {") {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || !trimmed.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        arms += 1;
        for ident in camel_idents(trimmed) {
            scanned.push((ident, format!("`RenameStep` arm `{trimmed}`")));
        }
    }

    // (b) Every column-zero `pub` type in `render/plan.rs` — the execution-side plan
    // vocabulary, all of it crate-root re-exported.
    let mut plan_types = 0usize;
    for line in PLAN_SRC.lines() {
        let Some(rest) = line
            .strip_prefix("pub struct ")
            .or_else(|| line.strip_prefix("pub enum "))
        else {
            continue;
        };
        let name = rest.split(['<', '(', ' ', '{']).next().unwrap_or(rest);
        plan_types += 1;
        scanned.push((name, "a `pub` type in `render/plan.rs`".to_string()));
    }

    let offenders: Vec<String> = scanned
        .iter()
        .filter(|(name, _)| !VENDOR_SHAPED.contains(name))
        .filter_map(|(name, site)| {
            VENDORS
                .iter()
                .find(|v| name.contains(*v))
                .map(|v| format!("`{name}` names `{v}` — {site}"))
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "core's lowered-plan vocabulary names a vendor:\n  {}\n\
         \n\
         This vocabulary is the part of core that SURVIVES the per-vendor crate \
         extraction: it is what a fourth backend would have to implement against, and \
         a vendor name in it tells that author the concept belongs to someone else. \
         It also tends to be false — `PgExpandContract` was constructed for MySQL — \
         because nothing enforces the vendor a name claims.\n\
         \n\
         Name the STRATEGY instead: `ExpandContract` (add the new shape, dual-write, \
         drop the old) against `TableRebuild` (create-copy-swap, for an engine with \
         no native `ALTER`). If the type is genuinely ONE vendor's — it models an \
         artifact no other engine has, the way `SqliteSequencePolicy` models \
         `sqlite_sequence` — then it SHOULD say so: add it to `VENDOR_SHAPED` above \
         with the artifact named. Backends themselves (`PostgresBackend`, \
         `MysqlBackend`, `SqliteBackend`) are correctly vendor-named and are not \
         scanned here.",
        offenders.join("\n  "),
    );

    assert!(
        arms >= 2 && plan_types >= 7,
        "scanned {arms} `RenameStep` arm(s) and {plan_types} `pub` type(s) in \
         render/plan.rs, expected at least 2 and 7\n\
         \n\
         This is the boundary self-check on the scans above, and it fires in two very \
         different situations. Either a scan broke — the `pub enum RenameStep {{` \
         needle, the `pub struct `/`pub enum ` prefixes, or the column-zero `}}` \
         region cut — in which case the check above iterated over nothing and its \
         green meant nothing; or a concept was deleted, which is a fine edit that \
         should lower the floor in the same commit, with the reason. Check which \
         before editing the numbers."
    );
}
