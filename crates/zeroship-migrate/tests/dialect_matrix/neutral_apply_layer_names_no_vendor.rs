//! Where a vendor's apply code may live, enforced rather than documented.
//!
//! `src/apply/` has two storeys and only one of them is allowed a vendor. The
//! files sitting DIRECTLY under `src/apply/` are the dialect-neutral apply layer
//! — the executor, the drift and baseline flows, the plan-precondition hoist —
//! and `src/apply/backend/mod.rs` is the neutral CONTRACT those flows call
//! through. Underneath it, `src/apply/backend/<dialect>/` is where a vendor is
//! supposed to be, and where all three vendors' session/journal/DML leaves
//! already are.
//!
//! The rule is invisible to every behaviour test. A PostgreSQL-only evaluator
//! parked in the neutral layer still evaluates PostgreSQL preconditions
//! correctly; a PostgreSQL-shaped struct declared on the neutral contract still
//! carries the right GUCs. What either costs is the crate extraction of
//! `docs/proposals/pluggable-backends.md`, and no assertion about behaviour can
//! see that. So the rule gets its own check or it has none.
//!
//! # What each half caught when it was written
//!
//! Both halves were RED on the tree that introduced them, which is the only
//! reason to believe either can see anything.
//!
//! [`the_neutral_apply_layer_names_no_vendor_backend`] caught
//! `apply/precondition.rs`: 685 production lines of `pg_query`,
//! `information_schema` and `&Client` whose own header called it "the POSTGRES
//! precondition impl", sitting in the neutral layer and naming
//! `PostgresBackend` three times in code. It moved to
//! `apply/backend/postgres/precondition.rs`, which was the only place it could go
//! at the time — it needs `SqlSession`/`ExecutorConfig`/`ApplyError`/`Migration`, and
//! all four were the engine's. All four moved down to `zero-migrate-backend`
//! afterwards, so the evaluator DID follow the renderers out: it is
//! `zeroship_migrate_postgres::backend::precondition` now, and that crate still does not
//! depend on the engine.
//!
//! [`the_backend_contract_declares_no_vendor_named_item`] caught
//! `PostgresSessionSnapshot`, declared on the neutral contract in
//! `apply/backend/mod.rs` while its own sibling `MysqlSessionSnapshot` was
//! declared in the MySQL backend's own `mod.rs` — the asymmetry being the tell. It
//! is genuinely vendor (its three fields are PostgreSQL GUCs; `SqliteBackend`'s
//! `SessionSnapshot` is `()`), so it kept a vendor name and moved to
//! `apply/backend/postgres/mod.rs` beside the sibling, spelled out the way the
//! dialect id spells it rather than abbreviated. (That file is
//! `zero-migrate-postgres/src/backend/mod.rs` now; the type went with it.)
//!
//! # What this does NOT catch
//!
//! A file in the neutral layer that is vendor-specific WITHOUT naming a vendor
//! backend type — PostgreSQL-only SQL reached through a neutral helper, say —
//! is invisible here, the same blindness `backend_modules_name_one_dialect.rs`
//! documents at length for its own check. A green run means "no neutral apply
//! file NAMES a vendor backend, and the contract DECLARES no vendor-named
//! item". It does not mean the neutral layer is free of vendor logic.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The three backend types. Naming one of these is the cheapest RELIABLE signal
/// that a file is a vendor's implementation rather than a neutral flow: the
/// neutral flows reach a backend only through `dyn MigrationBackend`, so they
/// have no reason to spell a concrete one, and a vendor impl almost always
/// constructs or bounds itself by its own.
const VENDOR_BACKENDS: &[&str] = &["PostgresBackend", "MysqlBackend", "SqliteBackend"];

/// Prefixes that make a DECLARED item a vendor's rather than the contract's.
///
/// `Pg` is listed alongside `Postgres` because the tree carried both, and the
/// short one is the spelling that hides: it does not match a search for the
/// dialect id, so it survives exactly the sweep that would find `Postgres`.
/// There is no `pg` dialect — `DialectId` spells it `postgres` — so a `Pg`-named
/// item is misnamed wherever it lives.
const VENDOR_ITEM_PREFIXES: &[&str] = &["Pg", "Postgres", "Mysql", "MySql", "Sqlite", "SqLite"];

/// Files the neutral-layer listing MUST contain.
///
/// A scan over a DISCOVERED set fails OPEN: point it at the wrong directory, or
/// let the read silently return nothing, and it passes with zero files
/// inspected. The floor below bounds HOW MANY were found; these bound WHICH, and
/// they stay true however far the layer shrinks. `executor.rs` is the neutral
/// apply flow itself and `mod.rs` is the layer's own root — neither can leave
/// `src/apply/` while there is a neutral apply layer at all.
const NEUTRAL_LAYER_ANCHORS: &[&str] = &["executor.rs", "mod.rs"];

/// The weaker of the two anti-blindness checks; see [`NEUTRAL_LAYER_ANCHORS`]
/// for the one that holds.
///
/// Deliberately low, and its premise is stated so the failure reads as a
/// question. This layer is EXPECTED to shrink as vendor code moves down into
/// `backend/<dialect>/`, so a count that falls is the project working. Lower it
/// when an extraction legitimately moved a file out, and name the extraction in
/// the commit; never lower it to silence a listing that broke, which is what the
/// anchors are for.
const NEUTRAL_LAYER_FLOOR: usize = 4;

/// Whether a source line is CODE rather than prose.
///
/// Line-oriented, and over-counts prose into code rather than the reverse: a
/// vendor name in a trailing `// …` on a code line still counts. That is the
/// safe direction for a check that is trying to prove an ABSENCE.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// The `.rs` files directly under `src/apply/`, NOT recursing — the whole point
/// is to separate that storey from `backend/<dialect>/` below it.
fn neutral_layer_files() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join("zero-migrate-core")
        .join("src")
        .join("apply");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "rs"))
        .collect();
    out.sort();
    out
}

/// A vendor implementation lives in that vendor's own crate, not in the neutral
/// apply layer.
///
/// It used to live under `apply/backend/<dialect>/`, one storey below this one, and
/// this test's job was to keep the two storeys apart. All three of those directories
/// have since become `zero-migrate-{postgres,sqlite,mysql}/src/backend/`, so the
/// destination is a crate rather than a subdirectory — but the rule over THIS layer
/// is unchanged and so is what a red means: a file here that needs a concrete backend
/// is a vendor's implementation wearing a neutral path.
#[test]
fn the_neutral_apply_layer_names_no_vendor_backend() {
    let files = neutral_layer_files();
    let names: BTreeSet<String> = files
        .iter()
        .map(|p| {
            p.file_name()
                .expect("a file")
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // The anchors run FIRST because they answer "did the listing see the layer at
    // all", which is the failure the floor is too blunt to catch.
    for anchor in NEUTRAL_LAYER_ANCHORS {
        assert!(
            names.contains(*anchor),
            "the listing found {names:?} under src/apply/ but never `{anchor}`, so it is \
             not seeing the layer it claims to check. Fix the listing. Do NOT delete the \
             anchor to get green — if `{anchor}` legitimately moved, anchor on another \
             file that cannot leave the neutral layer, and say which in the commit."
        );
    }
    assert!(
        files.len() >= NEUTRAL_LAYER_FLOOR,
        "the listing found only {} files directly under src/apply/, below the floor of \
         {NEUTRAL_LAYER_FLOOR}. The anchors above PASSED, so this is most likely the \
         layer legitimately shrinking as vendor code moves into backend/<dialect>/ — \
         lower the floor and name the extraction that did it.",
        files.len()
    );

    for path in &files {
        let rel = format!(
            "apply/{}",
            path.file_name().expect("a file").to_string_lossy()
        );
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for backend in VENDOR_BACKENDS {
            let hits = text
                .lines()
                .filter(|l| is_code(l))
                .filter(|l| l.contains(backend))
                .count();
            assert_eq!(
                hits, 0,
                "{rel} names `{backend}` on {hits} code line(s), but it sits in the \
                 DIALECT-NEUTRAL apply layer, which reaches a backend only through \
                 `dyn MigrationBackend`. A file that needs a concrete backend is that \
                 vendor's implementation and belongs in that vendor's crate, under \
                 `zero-migrate-<dialect>/src/backend/`, with the rest of it."
            );
        }
    }
}

/// The neutral backend contract declares the SEAM; each vendor declares its own
/// types beside its own impl.
///
/// `mod` declarations are exempt and that was not a loophole: owning
/// `mod postgres` / `mod mysql` / `mod sqlite` was precisely that file's job while
/// the vendors lived there, and it named all three symmetrically. It owns NONE of
/// them now — every execution half is in its own crate and core re-exports none —
/// so the exemption covers nothing today and stays only because a `mod` declaration
/// is still not a vendor-named item.
#[test]
fn the_backend_contract_declares_no_vendor_named_item() {
    const CONTRACT: &str = "apply/backend/mod.rs";
    let src = include_str!("../../../zero-migrate-core/src/apply/backend/mod.rs");

    let mut offenders: Vec<(usize, String)> = Vec::new();
    for (i, line) in src.lines().enumerate() {
        if !is_code(line) {
            continue;
        }
        let mut tokens = line.split_whitespace().peekable();
        // Step past the visibility, if any.
        if tokens.peek().is_some_and(|t| t.starts_with("pub")) {
            tokens.next();
        }
        // ...and past the modifiers an item can carry before its keyword.
        while tokens
            .peek()
            .is_some_and(|t| matches!(*t, "unsafe" | "async" | "default" | "extern"))
        {
            tokens.next();
        }
        let Some(keyword) = tokens.next() else {
            continue;
        };
        // `mod` is absent on purpose; see this test's doc comment.
        if !matches!(
            keyword,
            "struct" | "enum" | "trait" | "type" | "const" | "static" | "fn" | "union"
        ) {
            continue;
        }
        let Some(name) = tokens.next() else {
            continue;
        };
        let name = name.trim_end_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
        if VENDOR_ITEM_PREFIXES.iter().any(|p| name.starts_with(p)) {
            offenders.push((i + 1, name.to_string()));
        }
    }

    assert!(
        offenders.is_empty(),
        "{CONTRACT} declares vendor-named item(s) {offenders:?}. It is the \
         DIALECT-NEUTRAL contract every backend implements; a type named after one \
         vendor belongs in that vendor's own module beside its impl, the way \
         `MysqlSessionSnapshot` is declared beside `MysqlBackend` in \
         zero-migrate-mysql/src/backend/mod.rs. Moving it \
         there is the fix — keeping the vendor name is correct once it lives in the \
         vendor's module, as long as the name is the one the dialect id uses."
    );
}
