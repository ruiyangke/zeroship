//! The engine's view of the DML render seam — and the ONE place a dialect becomes a
//! vendor.
//!
//! The seam itself is `zero_migrate_backend::dml`, glob-re-exported below so every
//! existing `render::dml::…` path in this crate resolves unchanged. What lives HERE
//! is the handful of doors that used to take a closed dialect identity and resolve a
//! renderer from it.
//!
//! # Why they could not stay down there
//!
//! `escape_quote_ident_for_dialect(ident, dialect)` was
//! `renderer(dialect).quote_ident(ident)`, and `renderer` was an exhaustive `match`
//! naming the three vendor statics. That single line is the whole crate cycle: the
//! registry has to sit ABOVE the vendors (it names all three) and the spelling seam
//! has to sit BELOW them (they call it), so no crate can hold both. It is the
//! "identifier seam" in its most compressed form — a vendor asking a registry to
//! hand the vendor back to itself.
//!
//! The contract crate's copies take a `&dyn DmlRenderer` instead. A vendor passes
//! `self` and the round trip disappears; the engine, which genuinely holds a
//! dialect identity and not a renderer, resolves once — here.
//!
//! # What this buys, beyond compiling
//!
//! While the closed enum still exists, these compatibility doors resolve it through
//! the open vendor registry and immediately pass a renderer to the neutral contract.
//! Vendor implementations never call back through these doors. The final enum-removal
//! cluster deletes the doors with their callers instead of adding a reverse
//! open-id-to-closed-enum bridge.

pub use zero_migrate_backend::dml::*;

use zero_migrate_backend::dml as seam;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::expr::Expr;
use zero_migrate_ir::ir::{IrScalar, IrValue};

use crate::render::backends::renderer;

/// EMIT an identifier in `dialect`'s own spelling, decided by that dialect's backend
/// rather than by a `format!` in the engine.
///
/// This is the door for anything that will be sent to a database. The other door,
/// the snapshot codec is for the normal form that is COMPARED rather than
/// executed; picking between them is the point of there being two.
pub(crate) fn escape_quote_ident_for_dialect(ident: &str, dialect: &DialectId) -> String {
    seam::escape_quote_ident_for_backend(ident, renderer(dialect))
}

/// Validate a trigger-body identifier and emit it in the selected spelling.
pub(crate) fn quote_bare_ident_for_dialect(
    what: &'static str,
    ident: &str,
    dialect: &DialectId,
) -> Result<String, DmlError> {
    seam::quote_bare_ident_for_backend(what, ident, renderer(dialect))
}

/// The fail-closed gate for an ENGINE-supplied identifier, emitted in `dialect`'s
/// spelling.
pub(crate) fn quote_ident_checked_for_dialect(
    ident: &str,
    dialect: &DialectId,
) -> Result<String, IdentQuoteError> {
    seam::quote_ident_checked_for_backend(ident, renderer(dialect))
}

/// Render an inline string literal in `dialect`'s spelling.
pub(crate) fn inline_string_literal(s: &str, dialect: &DialectId) -> String {
    seam::inline_string_literal_for_backend(s, renderer(dialect))
}

/// Render an inline scalar literal in `dialect`'s spelling.
pub(crate) fn inline_literal(s: &IrScalar, dialect: &DialectId) -> Result<String, DmlError> {
    seam::inline_literal_for_backend(s, renderer(dialect))
}

/// Render a closed-AST expression to inline SQL for `dialect`.
pub(crate) fn render_expr_inline(expr: &Expr, dialect: &DialectId) -> Result<String, DmlError> {
    seam::render_expr_inline_for_backend(expr, renderer(dialect))
}

/// [`render_expr_inline`] with a caller-supplied column-reference spelling.
pub(crate) fn render_expr_inline_with_col<F>(
    expr: &Expr,
    dialect: &DialectId,
    col_ref: &F,
) -> Result<String, DmlError>
where
    F: Fn(&str) -> Result<String, DmlError>,
{
    seam::render_expr_inline_with_col_for_backend(expr, renderer(dialect), col_ref)
}

/// The columns a closed-AST expression reads, spelled for `dialect`.
pub(crate) fn expr_column_refs(expr: &Expr, dialect: &DialectId) -> Result<Vec<String>, DmlError> {
    seam::expr_column_refs_for_backend(expr, renderer(dialect))
}

/// Assemble an `insert` into a template + binds for `dialect`.
pub fn assemble_insert(
    project_schema: &str,
    dialect: &DialectId,
    table: &str,
    columns: &[String],
    rows: &[Vec<IrValue>],
    on_conflict: Option<&OnConflict>,
) -> Result<AssembledDml, DmlError> {
    seam::assemble_insert_for_backend(
        project_schema,
        renderer(dialect),
        table,
        columns,
        rows,
        on_conflict,
    )
}

/// Assemble an `update` into a template + binds for `dialect`.
pub fn assemble_update(
    project_schema: &str,
    dialect: &DialectId,
    table: &str,
    set: &std::collections::BTreeMap<String, IrValue>,
    r#where: Option<&Expr>,
) -> Result<AssembledDml, DmlError> {
    seam::assemble_update_for_backend(project_schema, renderer(dialect), table, set, r#where)
}

/// Assemble a `delete` into a template + binds for `dialect`.
pub fn assemble_delete(
    project_schema: &str,
    dialect: &DialectId,
    table: &str,
    r#where: &Expr,
    limit: Option<u64>,
) -> Result<AssembledDml, DmlError> {
    seam::assemble_delete_for_backend(project_schema, renderer(dialect), table, r#where, limit)
}

/// [`assemble_delete`], with a catalog-proven row identity for a limited delete.
pub(crate) fn assemble_delete_with_catalog_identity(
    project_schema: &str,
    dialect: &DialectId,
    table: &str,
    r#where: &Expr,
    limit: Option<u64>,
    catalog_identity_columns: Option<&[String]>,
) -> Result<AssembledDml, DmlError> {
    seam::assemble_delete_with_catalog_identity_for_backend(
        project_schema,
        renderer(dialect),
        table,
        r#where,
        limit,
        catalog_identity_columns,
    )
}

/// Assemble a `backfill`'s SET/WHERE clauses for `dialect`.
pub fn assemble_backfill_clauses(
    dialect: &DialectId,
    table: &str,
    set: &std::collections::BTreeMap<String, IrValue>,
    filter: Option<&Expr>,
) -> Result<BackfillClauses, DmlError> {
    seam::assemble_backfill_clauses_for_backend(renderer(dialect), table, set, filter)
}

/// [`assemble_backfill_clauses`], admitting an empty `set`.
pub(crate) fn assemble_backfill_clauses_allow_empty(
    dialect: &DialectId,
    table: &str,
    set: &std::collections::BTreeMap<String, IrValue>,
    filter: Option<&Expr>,
) -> Result<BackfillClauses, DmlError> {
    seam::assemble_backfill_clauses_allow_empty_for_backend(renderer(dialect), table, set, filter)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use zero_migrate_backend::dml::{render_expr_bound, BindCtx};
    use zero_migrate_backend::step::BindValue;
    use zero_migrate_ir::dialect::{MYSQL, POSTGRES, SQLITE};
    use zero_migrate_ir::expr::{
        BinaryOp, Expr, ExtractField, ScalarFn, SynthFn, UnaryOp,
    };

    const SCHEMA: &str = "app_proj";

    fn quote_ident_checked(ident: &str) -> Result<String, IdentQuoteError> {
        quote_ident_checked_for_dialect(ident, &POSTGRES)
    }

    /// A `BindCtx` RESOLVES its backend once, at construction, and carries the
    /// registry's own object for its dialect.
    ///
    /// The comparison is POINTER IDENTITY against
    /// [`crate::render::backends::renderer`], not behavioural agreement, because
    /// behavioural agreement is exactly what a re-lookup would also satisfy. What
    /// this pins is that `BindCtx::backend` and `BindCtx::dialect` cannot drift:
    /// a future constructor that resolved the wrong dialect, or a `backend` field
    /// wired to a hand-built renderer that bypasses the registry, fails here even
    /// though every emitted byte would still match.
    #[test]
    fn bind_ctx_resolves_its_backend_once_from_its_dialect() {
        for dialect in [&POSTGRES, &SQLITE, &MYSQL] {
            let ctx = BindCtx::new(renderer(dialect));
            let carried = std::ptr::from_ref(ctx.backend).cast::<u8>();
            let registry =
                std::ptr::from_ref(crate::render::backends::renderer(dialect)).cast::<u8>();
            assert_eq!(
                carried, registry,
                "BindCtx::new({dialect:?}) must carry the registry's backend for that dialect"
            );
        }
    }

    // ---- the ONE shared engine identifier seam --------------------------------

    /// `quote_ident_checked` fails CLOSED on the two bytes `"`-doubling
    /// cannot neutralise: an empty string and a NUL byte. Without the guard, the
    /// peer seams (`author`/`backfill`/`role`/`journal`) would
    /// have ACCEPTED a NUL and emitted `"a\0b"`.
    #[test]
    fn quote_ident_checked_fails_closed_on_empty_and_nul() {
        assert!(quote_ident_checked("").is_err(), "empty must fail closed");
        assert!(quote_ident_checked("a\0b").is_err(), "NUL must fail closed");
        assert_eq!(quote_ident_checked("").unwrap_err().reason, "empty");
        assert_eq!(
            quote_ident_checked("a\0b").unwrap_err().reason,
            "contains NUL"
        );
    }

    /// For any non-empty / non-NUL identifier the output is
    /// byte-identical to the bare `format!("\"{}\"", x.replace('"', "\"\""))` the
    /// peers used, including a quote-bearing schema (the dml goldens stay green).
    #[test]
    fn quote_ident_checked_is_byte_identical_to_bare_format() {
        for s in [
            "app_proj",
            "019efd94-1a2b-7000-8000-000000000000",
            "a\"b",
            "\"\"",
        ] {
            assert_eq!(
                quote_ident_checked(s).unwrap(),
                format!("\"{}\"", s.replace('"', "\"\"")),
                "byte-identity for {s:?}"
            );
        }
        // explicit quote-doubling spot-check
        assert_eq!(quote_ident_checked("a\"b").unwrap(), "\"a\"\"b\"");
    }

    /// The engine peer seams (`author`/`backfill`/`journal`)
    /// now all route through `quote_ident_checked`, so they emit BYTE-IDENTICAL
    /// output for the same quote-bearing schema (the "uniform render seam"
    /// requirement). The peers wrap the shared helper, so comparing each to the
    /// canonical helper proves the uniformity for all of them.
    ///
    /// `role` USED TO BE A LEG HERE and is not one any more, because the migrator
    /// role name derivation left this crate for the PostgreSQL backend. The leg
    /// went WITH it — `zero_migrate_postgres::role::tests::
    /// the_role_seam_renders_uniformly_and_fails_closed` asserts the same two
    /// facts (byte-identical escape-and-quote, fail-closed on empty/NUL) against
    /// the same shared helper. The invariant did not get dropped; it got a home
    /// next to its subject, which is the only place it can still see it.
    ///
    /// `journal` went the SAME way, and for the same reason, when the PostgreSQL
    /// execution half followed the role derivation out of this crate:
    /// `zero_migrate_postgres::backend::journal_sql`'s
    /// `the_journal_seam_renders_uniformly_and_fails_closed` is that leg now. What
    /// is left here is the ENGINE's own seam, which is the only one this crate can
    /// still see — and that is why this file no longer names a vendor backend
    /// module at all.
    #[test]
    fn all_engine_seams_render_uniformly() {
        let schema = "ap\"p"; // a quote-bearing engine schema
        let canonical = quote_ident_checked(schema).unwrap();
        // author (infallible-on-valid wrapper) — maps to its own error on failure.
        assert_eq!(
            crate::plan::author::quote_ident_for_test(schema).unwrap(),
            canonical
        );
        // …and it fails closed uniformly on a NUL too.
        assert!(crate::plan::author::quote_ident_for_test("a\0b").is_err());
    }

    /// The BACKTICK half of the same invariant, and it is a separate test rather
    /// than a second needle in the one below because the two spellings have
    /// different homes on purpose.
    ///
    /// MySQL's backtick-doubling escape must live in EXACTLY one physical home,
    /// `zero-migrate-mysql/src/dml.rs`, and nowhere else in production crate source.
    /// The DDL character-stream translator has one separately named exemption for
    /// the doubled-backtick literal, but not for the primitive escape call.
    ///
    /// WHY THIS TEST EXISTS AT ALL, given that nothing was mis-emitted before it.
    /// The backtick spelling used to live in `schema::query::mysql_quote_ident`,
    /// which was `pub` and in CORE, with `backends::mysql` reaching INTO core to
    /// get its own spelling — the exact mirror image of the ANSI arrangement. No
    /// emitted byte was wrong, because every call site named MySQL in the callee's
    /// name, and the one-dialect-literal test passed because the reach was by
    /// function name rather than a `DialectId` constant. The defect was
    /// STRUCTURAL and it was a step-4 blocker: the future `zero-migrate-mysql`
    /// would have needed core at runtime to spell its own identifier, which is the
    /// core-to-backend cycle the whole backend split exists to break.
    ///
    /// A second home also existed where nobody was looking for one:
    /// `zero_migrate_mysql::backend::journal_sql::quote_ident_mysql` carried its own copy
    /// of the same escape. The ANSI needle has zero offenders crate-wide, so the
    /// backtick needle having two was the asymmetry, not a difference of kind.
    ///
    /// TWO NEEDLES, AND THE SECOND ONE HAS AN EXEMPTION THAT IS ITSELF THE POINT.
    /// Needle 1 is the escape CALL. Needle 2 is the doubled-backtick string literal
    /// that a hand-rolled re-quoter emits without ever calling `replace`, and it has
    /// exactly one sanctioned occurrence: `zero-migrate-mysql::ddl::mysql_requote_sql`, the
    /// documented single translation point from the `pg_get_constraintdef` normal
    /// form into MySQL spelling. That function is a character-stream TRANSLATOR, not
    /// a spelling primitive — the MySQL counterpart of the constraint-definition
    /// codec's
    /// normal-form role rather than of `ansi_double_quote_ident`'s spelling role —
    /// and it is now owned by the backend whose constraint DDL it translates. It is
    /// exempted BY FILE rather than left unscanned, so
    /// a second hand-rolled re-quoter appearing anywhere else goes red.
    ///
    /// WHAT NEITHER NEEDLE CATCHES, and the limitation is the same shape as the ANSI
    /// scan's. Both are byte-patterns, so a bare wrap with no doubling at all —
    /// ``format!("`{ident}`")`` after a strict bare-identifier gate, which is what
    /// `zero_migrate_mysql::backend::backfill_sql::quote_bare` used to be — passes both
    /// while being an unrouted spelling. That site was routed by hand; only the
    /// compile-time half (the primitive being unnameable outside its backend module)
    /// generalises. The in-crate test expectations that build a backtick literal to
    /// CHECK an emitter are deliberately left alone: a probe that derives its
    /// expectation from the emitter it checks is not an oracle.
    #[test]
    fn no_bare_backtick_escape_seam_outside_the_mysql_backend() {
        use std::path::Path;
        // Both byte-patterns, assembled so this file does not itself contain
        // needle 1. `render/dml.rs` is exempt anyway, for these fragments and the
        // prose above; no `dml.rs` code performs the escape.
        let escape_call = ['r', 'e', 'p', 'l', 'a', 'c', 'e']
            .iter()
            .collect::<String>()
            + "('`', \"``\")";
        let doubled_literal = "\"``\"";
        let crates_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("zero-migrate lives under crates");
        let mut offenders: Vec<String> = Vec::new();
        let mut escape_home_hits = 0;
        let mut escape_literal_hits = 0;
        let mut requote_home_hits = 0;
        let mut stack = std::fs::read_dir(crates_root)
            .expect("read crates root")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("src"))
            .filter(|src| src.is_dir())
            .collect::<Vec<_>>();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(crates_root)
                    .unwrap()
                    .display()
                    .to_string();
                if rel == "zero-migrate/src/render/dml.rs" {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read src file");
                // The single sanctioned home of the escape CALL is the MySQL
                // backend module itself.
                if body.contains(&escape_call) {
                    if rel == "zero-migrate-mysql/src/dml.rs" {
                        escape_home_hits += body.matches(&escape_call).count();
                    } else {
                        offenders.push(format!("{rel} (escape call)"));
                    }
                }
                // The single sanctioned emitter of the doubled literal is the
                // normal-form translator; see this test's header.
                if body.contains(doubled_literal) {
                    if rel == "zero-migrate-mysql/src/dml.rs" {
                        // The primitive's one escape call necessarily contains it.
                        escape_literal_hits += body.matches(doubled_literal).count();
                    } else if rel == "zero-migrate-mysql/src/ddl.rs" {
                        requote_home_hits += body.matches(doubled_literal).count();
                    } else {
                        offenders.push(format!("{rel} (doubled literal)"));
                    }
                }
            }
        }
        offenders.sort();
        assert_eq!(
            escape_home_hits, 1,
            "the scan's positive control expected exactly one backtick escape call \
             in zero-migrate-mysql/src/dml.rs, found {escape_home_hits}"
        );
        assert_eq!(
            escape_literal_hits, 1,
            "the primitive home must contain exactly the one doubled literal its \
             escape call needs, found {escape_literal_hits}"
        );
        assert_eq!(
            requote_home_hits, 1,
            "the scan's positive control expected exactly one hand-rolled doubled \
             literal in zero-migrate-mysql/src/ddl.rs, found {requote_home_hits}"
        );
        assert!(
            offenders.is_empty(),
            "bare backtick spelling found outside zero-migrate-mysql — route \
             these through dml::escape_quote_ident_for_dialect(.., &MYSQL) \
             so the MySQL backend decides its own spelling: {offenders:?}"
        );
    }

    /// STRUCTURAL enforcement of the "no remaining bare
    /// `format!`/`replace` escape seam" claim. The raw `"` → `""` escape logic
    /// (`replace('"', "\"\"")`) must live in EXACTLY one physical home — and
    /// nowhere else in the crate source. Every other quoting seam routes through
    /// it (via one of the two `dml` doors for author-validated helpers, or via
    /// [`quote_ident_checked_for_dialect`] for the fail-closed engine-identifier
    /// surfaces).
    ///
    /// THE HOME MOVED, AND THE INVARIANT DID NOT WEAKEN. It used to be `dml.rs`.
    /// It is now `render/backends/mod.rs::ansi_double_quote_ident`, which is
    /// `pub(in crate::render::backends)` — so the new home is strictly STRONGER
    /// than the old one: "exactly one file contains these bytes" is now backed by
    /// "and no module outside `render::backends` can even name the function". This
    /// test going red on the move was expected and the fix was to retarget the
    /// exemption, not to relax the scan.
    ///
    /// `dml.rs` keeps its exemption only because this test's own needle strings and
    /// the prose above spell the pattern; no `dml.rs` code performs the escape.
    ///
    /// THE `schema/` EXEMPTION IS GONE, AND IT WAS LOAD-BEARING WHILE IT LASTED. The
    /// scan used to skip the whole `schema/` subtree on the grounds that the
    /// schema-authority DDL layer "carries its OWN identifier-quoting primitive
    /// (`schema::query::quote_ident`)" and was a distinct module layer from the render
    /// seam. That reasoning is exactly the shape of the defect this test exists to
    /// catch: `schema::query::quote_ident` was `pub`, took no dialect, and both
    /// `PostgresSchemaRenderer::foreign_key_target` AND
    /// `SqliteSchemaRenderer::foreign_key_target` spelled identifiers through it — so
    /// SQLite's schema renderer emitted through a `format!` in core that named no
    /// vendor, and was byte-correct only because two of the three shipping dialects
    /// agree on `"x"`.
    ///
    /// MEASURED, on the `--lib` binary, by neutering
    /// `render::backends::ansi_double_quote_ident` with one appended token: 125 red,
    /// of which exactly ONE was under `schema::`. Neutering
    /// `schema::query::quote_ident` instead reddened 39, and those 39 were DISJOINT
    /// from the 125 — a whole test population that could not observe the crate's
    /// single quoting home. After routing, the same one-token neuter reddens 164.
    ///
    /// The exemption is deleted rather than retargeted, so `schema/` is now scanned
    /// like every other subtree. `schema::query::mysql_quote_ident` used to survive
    /// this scan legitimately — it spells backticks, not `"` — and an earlier version
    /// of this note added that it was "the MySQL backend's own primitive
    /// (`backends::mysql` delegates to it) rather than a second home for anything".
    /// The delegation was real and the conclusion did not follow: a backend
    /// delegating INTO core is the mirror image of the arrangement this test
    /// enforces, not an instance of it. That function is now gone and the backtick
    /// spelling has its own scan, `no_bare_backtick_escape_seam_outside_the_mysql_backend`
    /// above.
    ///
    /// The `"` → `""` escape logic must NOT recur inline across sites such as
    /// `executor` / `precondition` / `baseline` / `expand_contract` / `shadow` /
    /// `declarative` / `db` / `render::lower` / `zero_migrate_sqlite::backend`.
    ///
    /// WHAT IT DOES NOT CATCH, MEASURED: the scan is a byte-pattern, so a
    /// re-implementation that spells the quote differently — `char::from(34)`,
    /// `'\u{22}'`, a `&str` const — passes it while being the identical defect.
    /// That is not hypothetical: the spike behind this seam wrote `char::from(34)`
    /// in a probe and evaded this guard by accident. The compile-time half (the
    /// primitive being unnameable outside `render::backends`) is what closes that
    /// gap, because it never looks at bytes at all.
    #[test]
    fn no_bare_escape_seam_outside_dml() {
        use std::path::Path;
        // The exact escape-call byte-pattern. We scan for the `replace` call that
        // doubles a double-quote; the ONLY legitimate occurrences live in dml.rs.
        let needle = ['r', 'e', 'p', 'l', 'a', 'c', 'e']
            .iter()
            .collect::<String>()
            + "('\"', \"\\\"\\\"\")";
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![src_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // The single sanctioned home is `render/backends/mod.rs` (the
                // primitive itself). `render/dml.rs` stays exempt for this test's
                // own needle strings and the prose that names the seam.
                let rel = path.strip_prefix(&src_root).unwrap().display().to_string();
                if rel == "render/backends/mod.rs" || rel == "render/dml.rs" {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read src file");
                if body.contains(&needle) {
                    offenders.push(path.strip_prefix(&src_root).unwrap().display().to_string());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "bare `\"`-escape seam found outside render/backends/mod.rs — route \
             these through dml::escape_quote_ident_for_dialect (to emit) / \
             snapshot::quote_constraint_definition_ident (the comparison normal form) / \
             dml::quote_ident_checked_for_dialect (engine identifiers): {offenders:?}"
        );
    }

    /// STRUCTURAL proof that the "every engine-supplied
    /// identifier render seam fail-closes" contract is TRUE, not just true for the
    /// five seams (`dml`/`role`/`author`/`backfill`/`journal`) that first adopted
    /// the wrapper. The infallible doors must NEVER be handed an
    /// **engine-supplied** identifier (project schema / migrator role / meta
    /// schema) — those must route through [`quote_ident_checked_for_dialect`] so they fail
    /// closed on empty / NUL. We scan the crate source for the give-away
    /// byte-patterns (`…(&cfg.confinement.meta_schema)`, `…(&cfg.project_schema)`,
    /// `…(role)`, `…(&exec_cfg.confinement.meta_schema)`) — every such site is an
    /// engine-identifier seam that must NOT use an infallible escaper.
    ///
    /// RETARGETED WITH THE SEAM. The infallible primitive used to be
    /// `dml::escape_quote_ident`, and these needles named it. That symbol no
    /// longer exists — the raw spelling moved into `render::backends` and became
    /// private to it — so needles built on the old name would have matched nothing
    /// forever after and this pin would have gone quietly dead while still passing.
    /// The needles now name the two doors that replaced it,
    /// [`escape_quote_ident_for_dialect`] and the constraint-definition codec, which is
    /// where an engine identifier could actually land today.
    ///
    /// Engine-identifier sites such as `precondition.rs` (project_schema + role),
    /// `executor.rs` (role + meta_schema ×4 + project_schema + recovery index),
    /// `baseline.rs` (meta_schema), and
    /// `db.rs::search_path_clause` (project/platform/extension schemas) must NOT
    /// feed an engine identifier to either door.
    ///
    /// **SCOPE — this is a PER-SITE regression pin, NOT a general invariant.** It
    /// only catches the exact call-site *spellings* in `needles` below (the give-away
    /// `(&cfg.…)` / `(role)` argument byte-patterns). A future engine-identifier
    /// seam bound to a *differently-named* variable — e.g.
    /// an indirect constraint-definition codec call would slip past this scan
    /// undetected. The broader, spelling-independent guarantee that NO bare `"`-escape
    /// seam exists outside `render/backends/mod.rs` is held by
    /// `no_bare_escape_seam_outside_dml` (above); this test complements it by naming
    /// the specific engine-identifier sites and proving they route through the
    /// fail-closed wrapper. When adding a new engine-identifier render seam, add its
    /// spelling to `needles` here.
    #[test]
    fn no_engine_identifier_uses_the_infallible_escaper() {
        use std::path::Path;
        // The engine-supplied identifier argument patterns. `quote_ident_checked_for_dialect`
        // takes the SAME args; the infallible doors must not.
        let esc = [
            'e', 's', 'c', 'a', 'p', 'e', '_', 'q', 'u', 'o', 't', 'e', '_', 'i', 'd', 'e', 'n',
            't', '_', 'f', 'o', 'r', '_', 'd', 'i', 'a', 'l', 'e', 'c', 't',
        ]
        .iter()
        .collect::<String>();
        let canon = [
            'p', 'g', '_', 'c', 'a', 'n', 'o', 'n', 'i', 'c', 'a', 'l', '_', 'i', 'd', 'e', 'n',
            't',
        ]
        .iter()
        .collect::<String>();
        let needles = [
            format!("{esc}(&cfg.confinement.meta_schema"),
            format!("{esc}(&cfg.project_schema"),
            format!("{esc}(&exec_cfg.confinement.meta_schema"),
            format!("{esc}(&exec_cfg.project_schema"),
            format!("{esc}(role"),
            format!("{canon}(&cfg.confinement.meta_schema)"),
            format!("{canon}(&cfg.project_schema)"),
            format!("{canon}(&exec_cfg.confinement.meta_schema)"),
            format!("{canon}(&exec_cfg.project_schema)"),
            format!("{canon}(role)"),
        ];
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![src_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // dml.rs holds only the helper + this test's needle literals.
                if path.file_name().and_then(|n| n.to_str()) == Some("dml.rs") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read src file");
                for needle in &needles {
                    if body.contains(needle.as_str()) {
                        offenders.push(format!(
                            "{} ({needle})",
                            path.strip_prefix(&src_root).unwrap().display()
                        ));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "engine-supplied identifier handed to the INFALLIBLE escaper — route \
             through dml::quote_ident_checked_for_dialect so it fails closed on empty/NUL: {offenders:?}"
        );
    }

    fn lit_str(s: &str) -> Expr {
        Expr::lit(IrScalar::Str(s.to_string()))
    }
    fn lit_int(i: i64) -> Expr {
        Expr::lit(IrScalar::Int(i))
    }
    fn val(s: IrScalar) -> IrValue {
        IrValue::Scalar(s)
    }
    fn dml_expr(e: Expr) -> IrValue {
        IrValue::Expr(e)
    }

    #[test]
    fn bytes_inline_literals_are_native_binary_values_on_every_dialect() {
        let value = IrScalar::Bytes(vec![0x00, 0x01, 0x7f, 0x80, 0xff]);
        assert_eq!(
            inline_literal(&value, &POSTGRES).unwrap(),
            "decode('AAF/gP8=', 'base64')"
        );
        assert_eq!(inline_literal(&value, &MYSQL).unwrap(), "(X'00017f80ff')");
        assert_eq!(inline_literal(&value, &SQLITE).unwrap(), "X'00017f80ff'");
    }

    // ── Concat is dialect-specific (regression: MySQL `||` is logical OR) ─────

    #[test]
    fn concat_renders_per_dialect_pg_sqlite_mysql() {
        // Regression guard: on MySQL, `||` is *logical OR*, so rendering `Concat`
        // as `a || b` there silently corrupts a string concat to a boolean. It
        // MUST render as `CONCAT(a, b)`. PG + SQLite keep the `||` operator.
        let expr = Expr::BinOp {
            op: BinaryOp::Concat,
            lhs: Box::new(Expr::col("first")),
            rhs: Box::new(Expr::col("last")),
        };

        let pg = render_expr_inline(&expr, &POSTGRES).unwrap();
        assert_eq!(
            pg, "(\"first\" || \"last\")",
            "PG uses the || concat operator"
        );

        let sqlite = render_expr_inline(&expr, &SQLITE).unwrap();
        assert_eq!(
            sqlite, "(\"first\" || \"last\")",
            "SQLite uses the || concat operator"
        );

        let mysql = render_expr_inline(&expr, &MYSQL).unwrap();
        assert!(
            mysql.starts_with("CONCAT(") && !mysql.contains("||"),
            "MySQL MUST render Concat as CONCAT(...), never `||` (logical OR): got {mysql}"
        );
    }

    #[test]
    fn cast_renders_per_dialect_type_names() {
        use zero_migrate_ir::expr::CastTarget;

        let cases = [
            (
                CastTarget::Int,
                "CAST(\"x\" AS integer)",
                "CAST(\"x\" AS integer)",
                "CAST(`x` AS signed)",
            ),
            (
                CastTarget::Bytes,
                "CAST(\"x\" AS bytea)",
                "CAST(\"x\" AS blob)",
                "CAST(`x` AS binary)",
            ),
            (
                CastTarget::Text,
                "CAST(\"x\" AS text)",
                "CAST(\"x\" AS text)",
                "CAST(`x` AS char)",
            ),
        ];

        for (target, pg, sqlite, mysql) in cases {
            let expr = Expr::Cast {
                operand: Box::new(Expr::col("x")),
                target,
            };
            assert_eq!(render_expr_inline(&expr, &POSTGRES).unwrap(), pg);
            assert_eq!(render_expr_inline(&expr, &SQLITE).unwrap(), sqlite);
            assert_eq!(render_expr_inline(&expr, &MYSQL).unwrap(), mysql);
        }
    }

    // ── Qualified column refs (the join-ON fix) ──────────────────────────────

    /// A qualified `ColRef { table, name }` renders `<table>.<col>` with the SAME
    /// per-dialect identifier quoting as an unqualified ref: PG/SQLite double-quote
    /// each half, MySQL backticks each half. An unqualified ref is unchanged.
    #[test]
    fn qualified_colref_renders_dotted_per_dialect() {
        let qualified = Expr::col_qualified("users", "id");
        assert_eq!(
            render_expr_inline(&qualified, &POSTGRES).unwrap(),
            "\"users\".\"id\"",
            "PG qualifies with double-quoted table.col"
        );
        assert_eq!(
            render_expr_inline(&qualified, &SQLITE).unwrap(),
            "\"users\".\"id\"",
            "SQLite qualifies with double-quoted table.col"
        );
        assert_eq!(
            render_expr_inline(&qualified, &MYSQL).unwrap(),
            "`users`.`id`",
            "MySQL qualifies with backtick-quoted table.col"
        );

        // Unqualified stays exactly as today — no table segment, no dot.
        let plain = Expr::col("id");
        assert_eq!(render_expr_inline(&plain, &POSTGRES).unwrap(), "\"id\"");
        assert_eq!(render_expr_inline(&plain, &SQLITE).unwrap(), "\"id\"");
        assert_eq!(render_expr_inline(&plain, &MYSQL).unwrap(), "`id`");

        // The parameterized (bind) path mirrors the inline path for the ColRef arm.
        assert_eq!(
            render_expr_bound(&qualified, &mut BindCtx::new(renderer(&POSTGRES))).unwrap(),
            "\"users\".\"id\""
        );
        assert_eq!(
            render_expr_bound(&qualified, &mut BindCtx::new(renderer(&MYSQL)),).unwrap(),
            "`users`.`id`"
        );
        assert_eq!(
            render_expr_bound(&plain, &mut BindCtx::new(renderer(&POSTGRES)),).unwrap(),
            "\"id\""
        );
    }

    #[test]
    fn length_is_char_length_on_mysql() {
        // Regression: MySQL LENGTH() is BYTE length; the portable length() intent
        // is CHARACTER length (PG/SQLite length()). MySQL MUST use CHAR_LENGTH().
        let expr = Expr::FnCall {
            r#fn: ScalarFn::Length,
            args: vec![Expr::col("name")],
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "length(\"name\")"
        );
        assert_eq!(
            render_expr_inline(&expr, &SQLITE).unwrap(),
            "length(\"name\")"
        );
        let mysql = render_expr_inline(&expr, &MYSQL).unwrap();
        assert!(
            mysql.starts_with("char_length("),
            "MySQL length() must render as CHAR_LENGTH (LENGTH is byte length): got {mysql}"
        );
    }

    /// Portable scalar functions keep equivalent semantics on every dialect.
    /// SQLite floor/ceil use core SQL because those named functions belong to an
    /// optional SQLite math extension.
    #[test]
    fn portable_scalar_fns_render_on_all_three() {
        let cases: &[(Expr, &str, Option<&str>)] = &[
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Round,
                    args: vec![Expr::col("x")],
                },
                "round(\"x\")",
                None,
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Round,
                    args: vec![Expr::col("x"), Expr::lit(IrScalar::Int(2))],
                },
                "round(\"x\", 2)",
                None,
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Floor,
                    args: vec![Expr::col("x")],
                },
                "floor(\"x\")",
                Some("(CASE WHEN \"x\" >= 9223372036854775808.0 OR \"x\" <= -9223372036854775808.0 THEN \"x\" ELSE CAST(\"x\" AS INTEGER) - (CAST(\"x\" AS INTEGER) > \"x\") END)"),
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Ceil,
                    args: vec![Expr::col("x")],
                },
                "ceil(\"x\")",
                Some("(CASE WHEN \"x\" >= 9223372036854775808.0 OR \"x\" <= -9223372036854775808.0 THEN \"x\" ELSE CAST(\"x\" AS INTEGER) + (CAST(\"x\" AS INTEGER) < \"x\") END)"),
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Substr,
                    args: vec![
                        Expr::col("s"),
                        Expr::lit(IrScalar::Int(1)),
                        Expr::lit(IrScalar::Int(3)),
                    ],
                },
                "substr(\"s\", 1, 3)",
                None,
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Replace,
                    args: vec![
                        Expr::col("s"),
                        Expr::lit(IrScalar::Str("a".into())),
                        Expr::lit(IrScalar::Str("b".into())),
                    ],
                },
                "replace(\"s\", 'a', 'b')",
                None,
            ),
        ];
        for (expr, pg_expect, sqlite_expect) in cases {
            assert_eq!(
                &render_expr_inline(expr, &POSTGRES).unwrap(),
                pg_expect,
                "PG render mismatch"
            );
            assert_eq!(
                &render_expr_inline(expr, &SQLITE).unwrap(),
                sqlite_expect.unwrap_or(pg_expect),
                "SQLite render mismatch"
            );
            // MySQL uses backtick identifiers and mode-independent UTF-8 hex
            // literals; the function name and argument shape stay equivalent.
            let mysql_expect = pg_expect
                .replace('"', "`")
                .replace("'a'", "_utf8mb4 X'61'")
                .replace("'b'", "_utf8mb4 X'62'");
            assert_eq!(
                render_expr_inline(expr, &MYSQL).unwrap(),
                mysql_expect,
                "MySQL render mismatch"
            );
        }
    }

    #[test]
    fn portable_extract_fields_render_equivalent_date_parts_on_all_three() {
        let cases = [
            (
                ExtractField::Year,
                "EXTRACT(year FROM \"ts\")",
                "CAST(strftime('%Y', \"ts\") AS INTEGER)",
                "EXTRACT(YEAR FROM `ts`)",
            ),
            (
                ExtractField::Month,
                "EXTRACT(month FROM \"ts\")",
                "CAST(strftime('%m', \"ts\") AS INTEGER)",
                "EXTRACT(MONTH FROM `ts`)",
            ),
            (
                ExtractField::Day,
                "EXTRACT(day FROM \"ts\")",
                "CAST(strftime('%d', \"ts\") AS INTEGER)",
                "EXTRACT(DAY FROM `ts`)",
            ),
            (
                ExtractField::Hour,
                "EXTRACT(hour FROM \"ts\")",
                "CAST(strftime('%H', \"ts\") AS INTEGER)",
                "EXTRACT(HOUR FROM `ts`)",
            ),
            (
                ExtractField::Minute,
                "EXTRACT(minute FROM \"ts\")",
                "CAST(strftime('%M', \"ts\") AS INTEGER)",
                "EXTRACT(MINUTE FROM `ts`)",
            ),
            (
                ExtractField::Dow,
                "EXTRACT(dow FROM \"ts\")",
                "CAST(strftime('%w', \"ts\") AS INTEGER)",
                "(DAYOFWEEK(`ts`) - 1)",
            ),
        ];

        for (field, pg, sqlite, mysql) in cases {
            let expr = Expr::Extract {
                field,
                from: Box::new(Expr::col("ts")),
            };
            assert_eq!(render_expr_inline(&expr, &POSTGRES).unwrap(), pg);
            assert_eq!(render_expr_inline(&expr, &SQLITE).unwrap(), sqlite);
            assert_eq!(render_expr_inline(&expr, &MYSQL).unwrap(), mysql);
        }
    }

    #[test]
    fn extract_fields_render_only_where_the_backend_admits_them() {
        let expr = Expr::Extract {
            field: ExtractField::Epoch,
            from: Box::new(Expr::col("ts")),
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "EXTRACT(epoch FROM \"ts\")"
        );
        for dialect in [&SQLITE, &MYSQL] {
            let err = render_expr_inline(&expr, dialect).unwrap_err();
            // Stricter than the old `contains("PostgreSQL-only")`, which was one
            // shared sentence every refusal produced: the refusal must now name
            // the FIELD it could not render, so a backend refusing the wrong part
            // no longer satisfies this.
            assert!(
                err.to_string().contains("epoch"),
                "an extract refusal must name the field it declined on {dialect:?}: {err}"
            );
        }

        let second = Expr::Extract {
            field: ExtractField::Second,
            from: Box::new(Expr::col("ts")),
        };
        assert_eq!(
            render_expr_inline(&second, &POSTGRES).unwrap(),
            "EXTRACT(second FROM \"ts\")",
            "PostgreSQL keeps fractional seconds, so it admits this field"
        );
    }

    #[test]
    fn regex_match_renders_postgres_and_mysql_but_refuses_sqlite() {
        let expr = Expr::RegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: "^a$".to_string(),
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "(\"name\" ~ '^a$'::text)"
        );
        assert_eq!(
            render_expr_inline(&expr, &MYSQL).unwrap(),
            "(`name` REGEXP _utf8mb4 X'5e6124')"
        );

        let err = render_expr_inline(&expr, &SQLITE).unwrap_err();
        assert!(
            err.to_string().contains("SQLite") && err.to_string().contains("REGEXP"),
            "SQLite regex must fail closed with a precise message: {err}"
        );
    }

    /// `c.fn.mod(a, b)` renders as the `%` OPERATOR — NOT `mod(...)` — on all three
    /// dialects. This is the one portable arithmetic fn whose spelling is an
    /// operator (SQLite has no `mod()` SQL function; `%` is universal).
    #[test]
    fn mod_renders_as_percent_operator_on_all_three() {
        let expr = Expr::FnCall {
            r#fn: ScalarFn::Mod,
            args: vec![Expr::col("n"), Expr::lit(IrScalar::Int(3))],
        };
        assert_eq!(render_expr_inline(&expr, &POSTGRES).unwrap(), "(\"n\" % 3)");
        assert_eq!(render_expr_inline(&expr, &SQLITE).unwrap(), "(\"n\" % 3)");
        assert_eq!(render_expr_inline(&expr, &MYSQL).unwrap(), "(`n` % 3)");
        // The bound (parameterized) path lowers identically (operator form).
        assert_eq!(
            render_expr_bound(&expr, &mut BindCtx::new(renderer(&POSTGRES)),).unwrap(),
            "(\"n\" % $1)"
        );
    }

    #[test]
    fn sqlite_inline_decimal_literals_preserve_authored_text() {
        let decimal = "12345678901234567890.1234567890";
        let literal = Expr::lit(IrScalar::Decimal(decimal.into()));
        assert_eq!(
            render_expr_inline(&literal, &SQLITE).unwrap(),
            format!("'{decimal}'")
        );
        assert_eq!(render_expr_inline(&literal, &POSTGRES).unwrap(), decimal);
        assert_eq!(render_expr_inline(&literal, &MYSQL).unwrap(), decimal);

        let list = Expr::InList {
            expr: Box::new(Expr::col("amount")),
            elems: vec![IrScalar::Decimal(decimal.into())],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&list, &SQLITE).unwrap(),
            format!("(\"amount\" IN ('{decimal}'))")
        );
    }

    #[test]
    fn is_true_is_false_rewritten_for_sqlite() {
        // SQLite has no IS TRUE / IS FALSE (no boolean type) — render as = 1 / = 0.
        // PG + MySQL keep the standard spelling.
        for (op, sqlite_expect, std_frag) in [
            (UnaryOp::IsTrue, "= 1", "IS TRUE"),
            (UnaryOp::IsFalse, "= 0", "IS FALSE"),
        ] {
            let e = Expr::UnaryOp {
                op,
                operand: Box::new(Expr::col("active")),
            };
            let pg = render_expr_inline(&e, &POSTGRES).unwrap();
            assert!(pg.contains(std_frag), "PG keeps `{std_frag}`: {pg}");
            let mysql = render_expr_inline(&e, &MYSQL).unwrap();
            assert!(
                mysql.contains(std_frag),
                "MySQL keeps `{std_frag}`: {mysql}"
            );
            let sqlite = render_expr_inline(&e, &SQLITE).unwrap();
            assert!(
                sqlite.contains(sqlite_expect)
                    && !sqlite.contains("IS TRUE")
                    && !sqlite.contains("IS FALSE"),
                "SQLite must rewrite `{std_frag}` to `{sqlite_expect}`: {sqlite}"
            );
        }
    }

    // ── portable predicate nodes: between / like / distinctFrom ──────────────

    #[test]
    fn between_renders_identically_on_all_three_dialects() {
        // `(operand BETWEEN low AND high)` is standard SQL — IDENTICAL on PG,
        // SQLite, and MySQL. The inline path binds no placeholders.
        let expr = Expr::Between {
            operand: Box::new(Expr::col("age")),
            low: Box::new(lit_int(18)),
            high: Box::new(lit_int(65)),
        };
        let expect_pg_sqlite = "(\"age\" BETWEEN 18 AND 65)";
        let expect_mysql = "(`age` BETWEEN 18 AND 65)";
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            expect_pg_sqlite
        );
        assert_eq!(
            render_expr_inline(&expr, &SQLITE).unwrap(),
            expect_pg_sqlite
        );
        assert_eq!(render_expr_inline(&expr, &MYSQL).unwrap(), expect_mysql);

        // Bound path: operand is an identifier; low/high become placeholders.
        for (dialect, ident) in [
            (&POSTGRES, "\"age\""),
            (&SQLITE, "\"age\""),
            (&MYSQL, "`age`"),
        ] {
            let mut ctx = BindCtx::new(renderer(dialect));
            let sql = render_expr_bound(&expr, &mut ctx).unwrap();
            assert!(
                sql.starts_with(&format!("({ident} BETWEEN ")) && sql.contains(" AND "),
                "BETWEEN keeps its shape on {dialect:?}: {sql}"
            );
            assert_eq!(ctx.binds.len(), 2, "low + high bind on {dialect:?}");
        }
    }

    #[test]
    fn like_renders_same_syntax_on_all_three_dialects() {
        // `(operand LIKE pattern)` — same syntax on PG, SQLite, MySQL. (Per-dialect
        // case-sensitivity semantics differ; this test proves the rendered SYNTAX
        // only, not semantic parity; see the Expr::Like doc comment.)
        let expr = Expr::Like {
            operand: Box::new(Expr::col("name")),
            pattern: Box::new(Expr::lit(IrScalar::Str("A%".to_string()))),
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "(\"name\" LIKE 'A%')"
        );
        assert_eq!(
            render_expr_inline(&expr, &SQLITE).unwrap(),
            "(\"name\" LIKE 'A%')"
        );
        assert_eq!(
            render_expr_inline(&expr, &MYSQL).unwrap(),
            "(`name` LIKE _utf8mb4 X'4125')"
        );
    }

    #[test]
    fn in_list_renders_pg_any_all_and_sql_in_not_in_on_all_three_dialects() {
        let includes = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("a".into()), IrScalar::Str("b".into())],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&includes, &POSTGRES).unwrap(),
            "(\"status\" = ANY (ARRAY['a'::text, 'b'::text]))"
        );
        assert_eq!(
            render_expr_inline(&includes, &SQLITE).unwrap(),
            "(\"status\" IN ('a', 'b'))"
        );
        assert_eq!(
            render_expr_inline(&includes, &MYSQL).unwrap(),
            "(`status` IN (_utf8mb4 X'61', _utf8mb4 X'62'))"
        );

        let excludes = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("x".into()), IrScalar::Str("y".into())],
            negated: true,
        };
        assert_eq!(
            render_expr_inline(&excludes, &POSTGRES).unwrap(),
            "(\"status\" <> ALL (ARRAY['x'::text, 'y'::text]))"
        );
        assert_eq!(
            render_expr_inline(&excludes, &SQLITE).unwrap(),
            "(\"status\" NOT IN ('x', 'y'))"
        );
        assert_eq!(
            render_expr_inline(&excludes, &MYSQL).unwrap(),
            "(`status` NOT IN (_utf8mb4 X'78', _utf8mb4 X'79'))"
        );

        let status_codes = Expr::InList {
            expr: Box::new(Expr::col("http_status")),
            elems: vec![IrScalar::Int(200), IrScalar::Int(404), IrScalar::Int(500)],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&status_codes, &POSTGRES).unwrap(),
            "(\"http_status\" = ANY (ARRAY[200,404,500]))"
        );
        assert_eq!(
            render_expr_inline(&status_codes, &SQLITE).unwrap(),
            "(\"http_status\" IN (200,404,500))"
        );
        assert_eq!(
            render_expr_inline(&status_codes, &MYSQL).unwrap(),
            "(`http_status` IN (200,404,500))"
        );

        let enabled = Expr::InList {
            expr: Box::new(Expr::col("enabled")),
            elems: vec![IrScalar::Bool(true), IrScalar::Bool(false)],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&enabled, &POSTGRES).unwrap(),
            "(\"enabled\" = ANY (ARRAY[TRUE,FALSE]))"
        );
        assert_eq!(
            render_expr_inline(&enabled, &SQLITE).unwrap(),
            "(\"enabled\" IN (TRUE,FALSE))"
        );
        assert_eq!(
            render_expr_inline(&enabled, &MYSQL).unwrap(),
            "(`enabled` IN (TRUE,FALSE))"
        );
    }

    #[test]
    fn in_list_empty_list_renders_boolean_constants() {
        let includes_empty = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![],
            negated: false,
        };
        let excludes_empty = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![],
            negated: true,
        };
        for dialect in [&POSTGRES, &SQLITE, &MYSQL] {
            assert_eq!(
                render_expr_inline(&includes_empty, dialect).unwrap(),
                "FALSE"
            );
            assert_eq!(
                render_expr_inline(&excludes_empty, dialect).unwrap(),
                "TRUE"
            );
            assert_eq!(
                render_expr_bound(&includes_empty, &mut BindCtx::new(renderer(dialect))).unwrap(),
                "FALSE"
            );
            assert_eq!(
                render_expr_bound(&excludes_empty, &mut BindCtx::new(renderer(dialect))).unwrap(),
                "TRUE"
            );
        }
    }

    #[test]
    fn in_list_escapes_text_elements() {
        let expr = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("a'b".into())],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "(\"status\" = ANY (ARRAY['a''b'::text]))"
        );
        assert_eq!(
            render_expr_inline(&expr, &SQLITE).unwrap(),
            "(\"status\" IN ('a''b'))"
        );
        assert_eq!(
            render_expr_inline(&expr, &MYSQL).unwrap(),
            "(`status` IN (_utf8mb4 X'612762'))"
        );
    }

    #[test]
    fn mysql_structural_string_literals_are_hex_in_inline_and_bound_paths() {
        let hostile = "a\\b'; DROP TABLE users; --";
        let hostile_hex = "615c62273b2044524f50205441424c452075736572733b202d2d";
        let expressions = [
            Expr::InList {
                expr: Box::new(Expr::col("status")),
                elems: vec![IrScalar::Str(hostile.into())],
                negated: false,
            },
            Expr::RegexMatch {
                expr: Box::new(Expr::col("status")),
                pattern: hostile.into(),
            },
        ];

        for expr in expressions {
            let inline = render_expr_inline(&expr, &MYSQL).unwrap();
            let mut ctx = BindCtx::new(renderer(&MYSQL));
            let bound = render_expr_bound(&expr, &mut ctx).unwrap();
            for sql in [&inline, &bound] {
                assert!(
                    sql.contains(&format!("_utf8mb4 X'{hostile_hex}'")),
                    "the author string must use the mode-independent hex renderer: {sql}"
                );
                assert!(
                    !sql.contains("DROP TABLE") && !sql.contains('\\'),
                    "no hostile source text may reach the SQL statement: {sql}"
                );
            }
            assert!(ctx.binds.is_empty(), "structural constants are not binds");
        }
    }

    #[test]
    fn in_list_rejects_mixed_and_bytes_elements() {
        let mixed = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("ok".into()), IrScalar::Int(200)],
            negated: false,
        };
        let err = render_expr_inline(&mixed, &POSTGRES).unwrap_err();
        assert!(
            err.to_string().contains("homogeneous"),
            "mixed inList should fail homogeneous check: {err}"
        );

        let bytes = Expr::InList {
            expr: Box::new(Expr::col("payload")),
            elems: vec![IrScalar::Bytes(vec![1, 2, 3])],
            negated: false,
        };
        let err = render_expr_inline(&bytes, &SQLITE).unwrap_err();
        assert!(
            err.to_string().contains("bytes are not allowed"),
            "bytes inList should fail closed: {err}"
        );
    }

    #[test]
    fn distinct_from_diverges_pg_sqlite_vs_mysql() {
        // The whole point of the node: PG + SQLite support `IS DISTINCT FROM`
        // directly; MySQL has no such operator, so the engine lowers it to
        // `NOT (x <=> y)` (`<=>` is MySQL's NULL-safe equality).
        let expr = Expr::DistinctFrom {
            left: Box::new(Expr::col("a")),
            right: Box::new(Expr::col("b")),
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "(\"a\" IS DISTINCT FROM \"b\")",
            "PG uses IS DISTINCT FROM"
        );
        assert_eq!(
            render_expr_inline(&expr, &SQLITE).unwrap(),
            "(\"a\" IS DISTINCT FROM \"b\")",
            "SQLite uses IS DISTINCT FROM"
        );
        assert_eq!(
            render_expr_inline(&expr, &MYSQL).unwrap(),
            "(NOT (`a` <=> `b`))",
            "MySQL lowers to NOT (a <=> b) — no IS DISTINCT FROM operator"
        );

        // Bound path renders the same divergent spellings.
        assert_eq!(
            render_expr_bound(&expr, &mut BindCtx::new(renderer(&POSTGRES)),).unwrap(),
            "(\"a\" IS DISTINCT FROM \"b\")"
        );
        assert_eq!(
            render_expr_bound(&expr, &mut BindCtx::new(renderer(&MYSQL)),).unwrap(),
            "(NOT (`a` <=> `b`))"
        );
    }

    // ── the Layer-2 dialect() per-dialect value escape ───────────────────────

    #[test]
    fn dialectal_renders_the_target_dialects_own_leg() {
        // dialect({ postgres: A, sqlite: B, mysql: C }) renders A on PostgreSQL,
        // B on SQLite, and C on MySQL — each target picks its OWN leg.
        let expr = Expr::Dialectal {
            legs: [
                (zero_migrate_ir::dialect::POSTGRES, Box::new(lit_str("A"))),
                (zero_migrate_ir::dialect::SQLITE, Box::new(lit_str("B"))),
                (zero_migrate_ir::dialect::MYSQL, Box::new(lit_str("C"))),
            ]
            .into_iter()
            .collect(),
        };
        // Inline path: each leg is an inline string literal.
        assert_eq!(render_expr_inline(&expr, &POSTGRES).unwrap(), "'A'");
        assert_eq!(render_expr_inline(&expr, &SQLITE).unwrap(), "'B'");
        assert_eq!(render_expr_inline(&expr, &MYSQL).unwrap(), "_utf8mb4 X'43'");

        // Bound path: each leg's literal becomes exactly ONE placeholder — the
        // shape is fixed by the chosen leg, not by the other legs.
        for (dialect, ph) in [(&POSTGRES, "$1"), (&SQLITE, "?1"), (&MYSQL, "?")] {
            let mut ctx = BindCtx::new(renderer(dialect));
            let sql = render_expr_bound(&expr, &mut ctx).unwrap();
            assert_eq!(sql, ph, "dialect() binds its chosen leg on {dialect:?}");
            assert_eq!(
                ctx.binds.len(),
                1,
                "exactly one leg's literal binds on {dialect:?}"
            );
        }
    }

    #[test]
    fn dialectal_recurses_into_the_chosen_leg_expression() {
        // A leg is a full Expr, not just a literal — the chosen leg renders
        // recursively (here a BETWEEN on PostgreSQL vs a bare column on SQLite).
        let expr = Expr::Dialectal {
            legs: [
                (
                    zero_migrate_ir::dialect::POSTGRES,
                    Box::new(Expr::Between {
                        operand: Box::new(Expr::col("age")),
                        low: Box::new(lit_int(1)),
                        high: Box::new(lit_int(9)),
                    }),
                ),
                (zero_migrate_ir::dialect::SQLITE, Box::new(Expr::col("age"))),
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            render_expr_inline(&expr, &POSTGRES).unwrap(),
            "(\"age\" BETWEEN 1 AND 9)",
        );
        assert_eq!(render_expr_inline(&expr, &SQLITE).unwrap(), "\"age\"");
    }

    #[test]
    fn dialectal_with_no_leg_for_target_is_a_fail_closed_render_backstop() {
        // A dialect({ postgres: A }) has no SQLite leg — validate refuses
        // this per-target BEFORE assembly, but the renderer is defensively
        // fail-closed rather than silently dropping the value.
        let expr = Expr::Dialectal {
            legs: [(zero_migrate_ir::dialect::POSTGRES, Box::new(lit_str("A")))]
                .into_iter()
                .collect(),
        };
        assert!(render_expr_inline(&expr, &POSTGRES).is_ok());
        let err = render_expr_inline(&expr, &SQLITE).unwrap_err();
        assert!(
            matches!(err, DmlError::UnrenderableExpr(_)),
            "no SQLite leg → fail-closed: {err:?}"
        );
    }

    // ── portable aggregate node: c.agg.count/sum/avg/min/max + DISTINCT ──────

    #[test]
    fn agg_renders_identically_on_all_three_dialects() {
        use zero_migrate_ir::expr::AggFunc;

        // count(*) — no arg — is byte-identical everywhere (no identifier at all).
        let count_star = Expr::Agg {
            func: AggFunc::Count,
            arg: None,
            delimiter: None,
            distinct: false,
        };
        for d in [&POSTGRES, &SQLITE, &MYSQL] {
            assert_eq!(
                render_expr_inline(&count_star, d).unwrap(),
                "count(*)",
                "count(*) is identical on {d:?}"
            );
        }

        // count(DISTINCT <col>) — only the identifier quoting differs (MySQL backticks).
        let count_distinct = Expr::Agg {
            func: AggFunc::Count,
            arg: Some(Box::new(Expr::col("x"))),
            delimiter: None,
            distinct: true,
        };
        assert_eq!(
            render_expr_inline(&count_distinct, &POSTGRES).unwrap(),
            "count(DISTINCT \"x\")"
        );
        assert_eq!(
            render_expr_inline(&count_distinct, &SQLITE).unwrap(),
            "count(DISTINCT \"x\")"
        );
        assert_eq!(
            render_expr_inline(&count_distinct, &MYSQL).unwrap(),
            "count(DISTINCT `x`)"
        );

        // sum/avg/min/max(<col>) — identical spelling, only quoting differs.
        for (func, name) in [
            (AggFunc::Sum, "sum"),
            (AggFunc::Avg, "avg"),
            (AggFunc::Min, "min"),
            (AggFunc::Max, "max"),
        ] {
            let e = Expr::Agg {
                func,
                arg: Some(Box::new(Expr::col("x"))),
                delimiter: None,
                distinct: false,
            };
            assert_eq!(
                render_expr_inline(&e, &POSTGRES).unwrap(),
                format!("{name}(\"x\")")
            );
            assert_eq!(
                render_expr_inline(&e, &SQLITE).unwrap(),
                format!("{name}(\"x\")")
            );
            assert_eq!(
                render_expr_inline(&e, &MYSQL).unwrap(),
                format!("{name}(`x`)")
            );
        }

        // The bound path renders the aggregate identically and binds no placeholders
        // (a ColRef arg is an identifier, not a bind).
        let mut ctx = BindCtx::new(renderer(&POSTGRES));
        assert_eq!(
            render_expr_bound(&count_distinct, &mut ctx).unwrap(),
            "count(DISTINCT \"x\")"
        );
        assert_eq!(ctx.binds.len(), 0, "a ColRef aggregate arg is not a bind");
        assert_eq!(
            render_expr_bound(&count_star, &mut BindCtx::new(renderer(&MYSQL)),).unwrap(),
            "count(*)"
        );
    }

    #[test]
    fn pg_first_aggregates_render_postgres_sql_names_and_string_agg_delimiter() {
        use zero_migrate_ir::expr::AggFunc;

        let string_agg = Expr::Agg {
            func: AggFunc::StringAgg,
            arg: Some(Box::new(Expr::col("name"))),
            delimiter: Some(Box::new(Expr::lit(IrScalar::Str(", ".to_string())))),
            distinct: false,
        };
        assert_eq!(
            render_expr_inline(&string_agg, &POSTGRES).unwrap(),
            "string_agg(\"name\", ', ')"
        );

        let string_agg_distinct = Expr::Agg {
            func: AggFunc::StringAgg,
            arg: Some(Box::new(Expr::col("name"))),
            delimiter: Some(Box::new(Expr::lit(IrScalar::Str("|".to_string())))),
            distinct: true,
        };
        assert_eq!(
            render_expr_inline(&string_agg_distinct, &POSTGRES).unwrap(),
            "string_agg(DISTINCT \"name\", '|')"
        );

        for (func, sql) in [
            (AggFunc::ArrayAgg, "array_agg(\"name\")"),
            (AggFunc::BoolAnd, "bool_and(\"name\")"),
            (AggFunc::BoolOr, "bool_or(\"name\")"),
        ] {
            let e = Expr::Agg {
                func,
                arg: Some(Box::new(Expr::col("name"))),
                delimiter: None,
                distinct: false,
            };
            assert_eq!(render_expr_inline(&e, &POSTGRES).unwrap(), sql);
        }
    }

    // ── identifier safety ───────────────────────────────────────────────────

    #[test]
    fn rejects_schema_qualified_table() {
        let err = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "other_schema.victims",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, DmlError::InvalidIdentifier { what: "table", .. }),
            "{err:?}"
        );
    }

    /// L1 self-defense: the PG `qualify_table` arm must not blindly trust
    /// the engine-supplied `project_schema`. A NUL byte — the one char that
    /// `"`-doubling cannot neutralise (PG rejects it inside an identifier) — is
    /// refused fail-closed with `DmlError::InvalidIdentifier { what: "schema" }`,
    /// not interpolated. RED before the `quote_schema` assertion landed (the old
    /// `format!` would have emitted a statement carrying the raw NUL).
    #[test]
    fn rejects_nul_in_project_schema_pg() {
        let err = assemble_insert(
            "app\0proj",
            &POSTGRES,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, DmlError::InvalidIdentifier { what: "schema", .. }),
            "{err:?}"
        );
    }

    /// An empty schema is likewise refused fail-closed — `""` cannot name a real
    /// relation and an empty quoted ident (`""`) is degenerate.
    #[test]
    fn rejects_empty_project_schema_pg() {
        let err = assemble_insert(
            "",
            &POSTGRES,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, DmlError::InvalidIdentifier { what: "schema", .. }),
            "{err:?}"
        );
    }

    /// The real Confined project schema is the app id — a UUIDv7 carrying `-`,
    /// which is NOT a bare `[A-Za-z_]…` ident. It MUST render (not be rejected):
    /// the prior over-strict `quote_ident` predicate would have broken every real
    /// deploy. `-` is render-safe, emitted verbatim inside the quoted schema.
    #[test]
    fn uuid_project_schema_renders_pg() {
        let a = assemble_insert(
            "019efd94-a4e0-7a82-8a08-95e1f906ca3f",
            &POSTGRES,
            "members",
            &["id".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"019efd94-a4e0-7a82-8a08-95e1f906ca3f\".\"members\" (\"id\") \
             VALUES ($1)"
        );
    }

    /// A hostile `"`-bearing schema cannot break out of the quoted identifier:
    /// it is SAFELY escaped (doubled `""`), not raw-interpolated and not
    /// (wrongly) rejected — the statement shape is unaltered, matching how every
    /// other engine seam quotes the schema.
    #[test]
    fn quote_bearing_project_schema_is_escaped_not_broken_out_pg() {
        let a = assemble_insert(
            "a\"; DROP--",
            &POSTGRES,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"a\"\"; DROP--\".\"t\" (\"a\") VALUES ($1)"
        );
    }

    #[test]
    fn rejects_injection_in_column() {
        let err = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "t",
            &["a\"); DROP TABLE users; --".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, DmlError::InvalidIdentifier { what: "column", .. }),
            "{err:?}"
        );
    }

    // ── insert: native binds, never interpolated ────────────────────────────

    #[test]
    fn insert_binds_all_values_pg() {
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![
                val(IrScalar::Int(200)),
                val(IrScalar::Str("ok".into())),
            ]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"status_codes\" (\"code\", \"label\") VALUES ($1, $2)"
        );
        assert_eq!(
            a.binds,
            vec![BindValue::Int(200), BindValue::Text("ok".into())]
        );
    }

    #[test]
    fn int64_above_js_safe_range_binds_and_renders_without_precision_loss() {
        let exact = 9_007_199_254_740_993_i64;
        let scalar = IrScalar::Int64(exact);

        for dialect in [&POSTGRES, &MYSQL, &SQLITE] {
            assert_eq!(
                inline_literal(&scalar, dialect).unwrap(),
                "9007199254740993"
            );
            let assembled = assemble_insert(
                SCHEMA,
                dialect,
                "events",
                &["id".into()],
                &[vec![val(scalar.clone())]],
                None,
            )
            .unwrap();
            assert_eq!(assembled.binds, vec![BindValue::Int(exact)]);
        }
    }

    #[test]
    fn insert_renders_exact_uuid_v4_without_bind_pg() {
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "events",
            &["created_at".into(), "id".into()],
            &[vec![
                IrValue::Expr(Expr::FnSynth {
                    r#fn: SynthFn::Now,
                    args: vec![],
                }),
                IrValue::Expr(Expr::UuidV4),
            ]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"events\" (\"created_at\", \"id\") VALUES (now(), gen_random_uuid())"
        );
        assert!(
            a.binds.is_empty(),
            "UUIDv4 insert value is DB-evaluated, not a bind"
        );
    }

    #[test]
    fn insert_renders_exact_uuid_v4_without_uuid_v1_mysql() {
        let a = assemble_insert(
            SCHEMA,
            &MYSQL,
            "events",
            &["created_at".into(), "id".into()],
            &[vec![
                IrValue::Expr(Expr::FnSynth {
                    r#fn: SynthFn::Now,
                    args: vec![],
                }),
                IrValue::Expr(Expr::UuidV4),
            ]],
            None,
        )
        .unwrap();
        assert!(
            a.template.contains("random_bytes"),
            "MySQL UUIDv4 must be synthesized from random bytes: {}",
            a.template
        );
        assert!(
            a.template.contains("hex((ord(random_bytes(1)) & 15) | 64)"),
            "MySQL UUIDv4 must pin the version bits to 0100: {}",
            a.template
        );
        assert!(
            a.template
                .contains("hex((ord(random_bytes(1)) & 63) | 128)"),
            "MySQL UUIDv4 must pin the RFC variant bits to 10: {}",
            a.template
        );
        assert!(
            !a.template.contains("UUID()"),
            "MySQL UUID() generates UUIDv1 and must never lower UUIDv4: {}",
            a.template
        );
        assert!(
            a.binds.is_empty(),
            "UUIDv4 insert value is DB-evaluated, not a bind"
        );
    }

    #[test]
    fn insert_renders_exact_uuid_v4_without_bind_sqlite() {
        let a = assemble_insert(
            SCHEMA,
            &SQLITE,
            "events",
            &["created_at".into(), "id".into()],
            &[vec![
                IrValue::Expr(Expr::FnSynth {
                    r#fn: SynthFn::Now,
                    args: vec![],
                }),
                IrValue::Expr(Expr::UuidV4),
            ]],
            None,
        )
        .unwrap();
        assert!(
            a.template.contains("'-4'"),
            "SQLite UUIDv4 must pin the version nibble: {}",
            a.template
        );
        assert!(
            a.template.contains("substr('89ab'"),
            "SQLite UUIDv4 must pin the RFC variant nibble: {}",
            a.template
        );
        assert!(
            a.binds.is_empty(),
            "UUIDv4 insert value is DB-evaluated, not a bind"
        );
    }

    #[test]
    fn sqlite_uuid_v4_samples_have_canonical_rfc_bits() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let expr = render_expr_inline(&Expr::UuidV4, &SQLITE).unwrap();
        let sql = format!("SELECT {expr}");
        let mut values = Vec::with_capacity(128);

        for _ in 0..128 {
            let value: String = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
            let bytes = value.as_bytes();
            assert_eq!(
                bytes.len(),
                36,
                "UUID must have canonical text length: {value}"
            );
            assert_eq!(&value[8..9], "-");
            assert_eq!(&value[13..14], "-");
            assert_eq!(&value[18..19], "-");
            assert_eq!(&value[23..24], "-");
            assert_eq!(bytes[14], b'4', "UUIDv4 version nibble: {value}");
            assert!(
                matches!(bytes[19], b'8' | b'9' | b'a' | b'b'),
                "UUID RFC variant nibble: {value}"
            );
            assert!(
                bytes.iter().enumerate().all(|(index, byte)| {
                    matches!(index, 8 | 13 | 18 | 23) || byte.is_ascii_hexdigit()
                }),
                "UUID must contain only hexadecimal digits and separators: {value}"
            );
            assert_eq!(value, value.to_ascii_lowercase());
            values.push(value);
        }

        // Every assertion above holds for an expression that returns one constant
        // well-formed v4 UUID 128 times, so none of them proves the rendered
        // expression generates anything. This one does: 128 evaluations must
        // yield 128 distinct values. It catches a constant or near-constant
        // generator and nothing more - 128 samples cannot detect bias, low
        // entropy, or a long repeat period.
        let distinct: std::collections::HashSet<&String> = values.iter().collect();
        assert_eq!(
            distinct.len(),
            values.len(),
            "128 evaluations of the rendered UUIDv4 expression must all differ"
        );
    }

    #[test]
    fn uuid_v7_is_native_postgres_and_fails_closed_elsewhere() {
        assert_eq!(
            render_expr_inline(&Expr::UuidV7, &POSTGRES).unwrap(),
            "uuidv7()"
        );
        for dialect in [&MYSQL, &SQLITE] {
            let error = render_expr_inline(&Expr::UuidV7, dialect).unwrap_err();
            assert!(
                matches!(error, DmlError::UnrenderableExpr(ref message) if message.contains("uuidV7") && message.contains("unsupported")),
                "unexpected {dialect:?} UUIDv7 error: {error:?}"
            );
        }
    }

    #[test]
    fn insert_uses_question_placeholders_on_sqlite() {
        let a = assemble_insert(
            SCHEMA,
            &SQLITE,
            "t",
            &["a".into(), "b".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Null)]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"t\" (\"a\", \"b\") VALUES (?1, ?2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(1), BindValue::Null]);
    }

    #[test]
    fn binary_insert_values_round_trip_without_text_coercion() {
        let bytes = vec![0, 1, 0x7f, 0x80, 0xff];
        let assemble = |dialect: &DialectId| {
            assemble_insert(
                SCHEMA,
                dialect,
                "files",
                &["payload".into()],
                &[vec![val(IrScalar::Bytes(bytes.clone()))]],
                None,
            )
            .unwrap()
        };

        let pg = assemble(&POSTGRES);
        assert!(pg.template.contains("VALUES (decode($1, 'base64'))"));
        assert_eq!(pg.binds, vec![BindValue::Text("AAF/gP8=".into())]);

        let mysql = assemble(&MYSQL);
        assert!(mysql.template.contains("VALUES (FROM_BASE64(?))"));
        assert_eq!(mysql.binds, vec![BindValue::Text("AAF/gP8=".into())]);

        let sqlite = assemble(&SQLITE);
        assert!(sqlite.template.contains("VALUES (?1)"));
        assert_eq!(sqlite.binds, vec![BindValue::Bytes(bytes)]);
    }

    #[test]
    fn insert_multi_row_continues_placeholder_counter() {
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))], vec![val(IrScalar::Int(2))]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"t\" (\"a\") VALUES ($1), ($2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(1), BindValue::Int(2)]);
    }

    /// Bind-safety: a value full of SQL metacharacters cannot alter the statement
    /// shape — it is a single bind, the template is unchanged.
    #[test]
    fn insert_metacharacter_value_cannot_alter_shape() {
        let hostile = "x'); DROP TABLE users; --";
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Str(hostile.into()))]],
            None,
        )
        .unwrap();
        // The template carries ONLY the placeholder; the hostile bytes are a bind.
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"t\" (\"a\") VALUES ($1)"
        );
        assert!(
            !a.template.contains("DROP"),
            "metacharacters must not reach the template"
        );
        assert_eq!(a.binds, vec![BindValue::Text(hostile.into())]);
    }

    #[test]
    fn insert_over_the_bind_param_ceiling_is_rejected() {
        // One column × (MAX_BIND_PARAMS + 1) rows assembles one bind per row,
        // overflowing the protocol parameter ceiling. Reject with a bounded error.
        let rows: Vec<Vec<IrValue>> = (0..=MAX_BIND_PARAMS as i64)
            .map(|i| vec![val(IrScalar::Int(i))])
            .collect();
        let err = assemble_insert(SCHEMA, &POSTGRES, "t", &["a".into()], &rows, None).unwrap_err();
        assert!(
            matches!(err, DmlError::TooManyBinds { count, max, .. } if count == MAX_BIND_PARAMS + 1 && max == MAX_BIND_PARAMS),
            "{err:?}"
        );
        // Exactly at the ceiling still assembles.
        let rows_ok: Vec<Vec<IrValue>> = (0..MAX_BIND_PARAMS as i64)
            .map(|i| vec![val(IrScalar::Int(i))])
            .collect();
        assert!(assemble_insert(SCHEMA, &POSTGRES, "t", &["a".into()], &rows_ok, None).is_ok());
    }

    #[test]
    fn insert_ragged_row_rejected() {
        let err = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "t",
            &["a".into(), "b".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::MalformedInsert { .. }), "{err:?}");
    }

    // Structured onConflict rendering across all three dialects.

    #[test]
    fn insert_on_conflict_renders_on_pg() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "label".to_string(),
                val(IrScalar::Str("dup".into())),
            )])),
        };
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"status_codes\" (\"code\", \"label\") VALUES ($1, $2) \
             ON CONFLICT (\"code\") DO UPDATE SET \"label\" = $3"
        );
        assert_eq!(
            a.binds,
            vec![
                BindValue::Int(1),
                BindValue::Text("ok".into()),
                BindValue::Text("dup".into())
            ]
        );
    }

    #[test]
    fn insert_on_conflict_do_update_renders_fnsynth_without_bind_pg() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "updated_at".to_string(),
                IrValue::Expr(Expr::FnSynth {
                    r#fn: SynthFn::Now,
                    args: vec![],
                }),
            )])),
        };
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"status_codes\" (\"code\", \"label\") VALUES ($1, $2) \
             ON CONFLICT (\"code\") DO UPDATE SET \"updated_at\" = now()"
        );
        assert_eq!(
            a.binds,
            vec![BindValue::Int(1), BindValue::Text("ok".into())],
            "fnSynth(now) in doUpdate must be DB-evaluated, not a bind"
        );
    }

    #[test]
    fn insert_on_conflict_do_nothing_pg() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: None,
        };
        let a = assemble_insert(
            SCHEMA,
            &POSTGRES,
            "t",
            &["code".into()],
            &[vec![val(IrScalar::Int(1))]],
            Some(&oc),
        )
        .unwrap();
        assert!(
            a.template.ends_with("ON CONFLICT (\"code\") DO NOTHING"),
            "{}",
            a.template
        );
    }

    #[test]
    fn insert_on_conflict_renders_exact_target_on_sqlite() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "label".to_string(),
                val(IrScalar::Str("dup".into())),
            )])),
        };
        let assembled = assemble_insert(
            SCHEMA,
            &SQLITE,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();
        assert_eq!(
            assembled.template,
            "INSERT INTO \"status_codes\" (\"code\", \"label\") VALUES (?1, ?2) \
             ON CONFLICT (\"code\") DO UPDATE SET \"label\" = ?3"
        );
        assert_eq!(
            assembled.binds,
            vec![
                BindValue::Int(1),
                BindValue::Text("ok".into()),
                BindValue::Text("dup".into()),
            ]
        );
    }

    #[test]
    fn insert_on_conflict_do_nothing_is_refused_on_mysql() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: None,
        };
        let err = assemble_insert(
            SCHEMA,
            &MYSQL,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::MySqlConflictDoNothingNotExact));
    }

    #[test]
    fn insert_on_conflict_guards_mysql_update_with_authored_target() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "label".to_string(),
                val(IrScalar::Str("dup".into())),
            )])),
        };
        let assembled = assemble_insert(
            SCHEMA,
            &MYSQL,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();
        assert_eq!(
            assembled.template,
            "INSERT INTO `app_proj`.`status_codes` (`code`, `label`) VALUES (?, ?) \
             AS `zero-migrate-incoming`(`zero-migrate-value-0`, `zero-migrate-value-1`) \
             ON DUPLICATE KEY UPDATE `label` = IF((`app_proj`.`status_codes`.`code` \
             = `zero-migrate-incoming`.`zero-migrate-value-0`), ?, \
             CONCAT('', JSON_EXTRACT('zero-migrate conflict target mismatch', '$')))"
        );
        assert_eq!(
            assembled.binds,
            vec![
                BindValue::Int(1),
                BindValue::Text("ok".into()),
                BindValue::Text("dup".into()),
            ]
        );
    }

    #[test]
    fn mysql_wrong_target_guard_does_not_depend_on_strict_sql_mode() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "label".to_string(),
                val(IrScalar::Str("dup".into())),
            )])),
        };
        let assembled = assemble_insert(
            SCHEMA,
            &MYSQL,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();

        assert!(
            assembled
                .template
                .contains("JSON_EXTRACT('zero-migrate conflict target mismatch', '$')"),
            "the false branch must use a MySQL expression that raises under every sql_mode: {}",
            assembled.template
        );
        assert!(
            !assembled.template.contains(" / "),
            "division by zero degrades to a warning and NULL under permissive sql_mode: {}",
            assembled.template
        );
    }

    #[test]
    fn insert_on_conflict_rejects_unguardable_mysql_target() {
        let missing_incoming = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "label".to_string(),
                val(IrScalar::Str("dup".into())),
            )])),
        };
        let err = assemble_insert(
            SCHEMA,
            &MYSQL,
            "status_codes",
            &["label".into()],
            &[vec![val(IrScalar::Str("ok".into()))]],
            Some(&missing_incoming),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DmlError::MySqlConflictTargetNotInserted { column, .. } if column == "code"
        ));

        let target_update = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "code".to_string(),
                val(IrScalar::Int(2)),
            )])),
        };
        let err = assemble_insert(
            SCHEMA,
            &MYSQL,
            "status_codes",
            &["code".into()],
            &[vec![val(IrScalar::Int(1))]],
            Some(&target_update),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DmlError::MySqlConflictTargetUpdated { column, .. } if column == "code"
        ));
    }

    #[test]
    fn mysql_conflict_update_rejects_cross_assignment_dependencies() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([
                ("first".to_string(), dml_expr(Expr::col("second"))),
                ("second".to_string(), dml_expr(Expr::col("first"))),
            ])),
        };

        let error = assemble_insert(
            SCHEMA,
            &MYSQL,
            "status_codes",
            &["code".into(), "first".into(), "second".into()],
            &[vec![
                val(IrScalar::Int(1)),
                val(IrScalar::Str("a".into())),
                val(IrScalar::Str("b".into())),
            ]],
            Some(&oc),
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                DmlError::MySqlCrossAssignmentDependency {
                    op,
                    column,
                    referenced_column,
                    ..
                } if *op == "onConflict.doUpdate"
                    && column == "first"
                    && referenced_column == "second"
            ),
            "unexpected error: {error:?}"
        );
    }

    // ── update: bound set + where, both dialects ─────────────────────────────

    #[test]
    fn update_binds_literal_in_set_and_where() {
        let set = BTreeMap::from([(
            "label".to_string(),
            dml_expr(Expr::FnCall {
                r#fn: ScalarFn::Coalesce,
                args: vec![Expr::col("label"), lit_str("unknown")],
            }),
        )]);
        let pred = Expr::BinOp {
            op: BinaryOp::Gt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let a = assemble_update(SCHEMA, &POSTGRES, "status_codes", &set, Some(&pred)).unwrap();
        assert_eq!(
            a.template,
            "UPDATE \"app_proj\".\"status_codes\" SET \"label\" = coalesce(\"label\", $1) \
             WHERE (\"code\" > $2)"
        );
        assert_eq!(
            a.binds,
            vec![BindValue::Text("unknown".into()), BindValue::Int(0)]
        );
    }

    #[test]
    fn update_portable_on_sqlite() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(lit_int(5)))]);
        let a = assemble_update(SCHEMA, &SQLITE, "t", &set, None).unwrap();
        assert_eq!(a.template, "UPDATE \"t\" SET \"a\" = ?1");
        assert_eq!(a.binds, vec![BindValue::Int(5)]);
    }

    #[test]
    fn update_empty_set_rejected() {
        let err = assemble_update(SCHEMA, &POSTGRES, "t", &BTreeMap::new(), None).unwrap_err();
        assert!(
            matches!(err, DmlError::EmptySet { op: "update", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn mysql_update_rejects_cross_assignment_dependencies_but_allows_self_reference() {
        let swap = BTreeMap::from([
            ("first".to_string(), dml_expr(Expr::col("second"))),
            ("second".to_string(), dml_expr(Expr::col("first"))),
        ]);
        let error = assemble_update(SCHEMA, &MYSQL, "t", &swap, None).unwrap_err();
        assert!(
            matches!(
                &error,
                DmlError::MySqlCrossAssignmentDependency {
                    op,
                    column,
                    referenced_column,
                    ..
                } if *op == "update" && column == "first" && referenced_column == "second"
            ),
            "unexpected error: {error:?}"
        );

        let increment = BTreeMap::from([(
            "counter".to_string(),
            dml_expr(Expr::BinOp {
                op: BinaryOp::Add,
                lhs: Box::new(Expr::col("counter")),
                rhs: Box::new(lit_int(1)),
            }),
        )]);
        assert!(
            assemble_update(SCHEMA, &MYSQL, "t", &increment, None).is_ok(),
            "a column's own RHS is evaluated before that assignment and remains portable"
        );
    }

    // ── delete: mandatory where, both dialects ───────────────────────────────

    #[test]
    fn delete_binds_where_pg() {
        let pred = Expr::UnaryOp {
            op: UnaryOp::IsNull,
            operand: Box::new(Expr::col("code")),
        };
        let a = assemble_delete(SCHEMA, &POSTGRES, "t", &pred, None).unwrap();
        assert_eq!(
            a.template,
            "DELETE FROM \"app_proj\".\"t\" WHERE (\"code\" IS NULL)"
        );
        assert!(a.binds.is_empty());
    }

    #[test]
    fn delete_with_limit_sqlite() {
        let pred = Expr::BinOp {
            op: BinaryOp::Lt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let identity = vec!["id".to_string()];
        let a = assemble_delete_with_catalog_identity(
            SCHEMA,
            &SQLITE,
            "t",
            &pred,
            Some(100),
            Some(&identity),
        )
        .unwrap();
        assert_eq!(
            a.template,
            "DELETE FROM \"t\" WHERE \"id\" IN \
             (SELECT \"id\" FROM \"t\" WHERE (\"code\" < ?1) LIMIT ?2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(0), BindValue::Int(100)]);
    }

    #[test]
    fn sqlite_limited_delete_does_not_treat_a_shadowing_rowid_column_as_identity() {
        let pred = Expr::BinOp {
            op: BinaryOp::Lt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let identity = vec!["id".to_string()];
        let assembled = assemble_delete_with_catalog_identity(
            SCHEMA,
            &SQLITE,
            "t",
            &pred,
            Some(1),
            Some(&identity),
        )
        .unwrap();

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, rowid INTEGER NOT NULL, code INTEGER NOT NULL);\
             INSERT INTO t (id, rowid, code) VALUES (1, 7, -1), (2, 7, -1), (3, 8, -1);",
        )
        .unwrap();
        let changed = conn
            .execute(&assembled.template, rusqlite::params![0_i64, 1_i64])
            .unwrap();

        assert_eq!(changed, 1, "a limit of one must identify exactly one row");
    }

    #[test]
    fn sqlite_limited_delete_uses_a_composite_key_on_without_rowid_tables() {
        let pred = Expr::BinOp {
            op: BinaryOp::Lt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let identity = vec!["tenant".to_string(), "id".to_string()];
        let assembled = assemble_delete_with_catalog_identity(
            SCHEMA,
            &SQLITE,
            "t",
            &pred,
            Some(1),
            Some(&identity),
        )
        .unwrap();
        assert_eq!(
            assembled.template,
            "DELETE FROM \"t\" WHERE (\"tenant\", \"id\") IN \
             (SELECT \"tenant\", \"id\" FROM \"t\" WHERE (\"code\" < ?1) LIMIT ?2)"
        );

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (\
                 tenant TEXT NOT NULL,\
                 id TEXT NOT NULL,\
                 code INTEGER NOT NULL,\
                 PRIMARY KEY (tenant, id)\
             ) WITHOUT ROWID;\
             INSERT INTO t (tenant, id, code) VALUES\
                 ('a', '1', -1), ('a', '2', -1), ('b', '1', -1);",
        )
        .unwrap();
        let changed = conn
            .execute(&assembled.template, rusqlite::params![0_i64, 1_i64])
            .unwrap();
        assert_eq!(changed, 1);
    }

    #[test]
    fn sqlite_limited_delete_without_catalog_identity_is_rejected() {
        let pred = Expr::UnaryOp {
            op: UnaryOp::IsNull,
            operand: Box::new(Expr::col("code")),
        };
        let err = assemble_delete(SCHEMA, &SQLITE, "t", &pred, Some(1)).unwrap_err();
        assert_eq!(
            err,
            DmlError::SqliteLimitedDeleteNeedsUniqueIdentity {
                table: "t".to_string()
            }
        );
    }

    #[test]
    fn delete_with_limit_pg_uses_tableoid_and_ctid_for_partitioned_parents() {
        // Child partitions can reuse the same ctid. Pair it with tableoid so the
        // outer delete targets only the physical row selected by the subquery.
        let pred = Expr::BinOp {
            op: BinaryOp::Lt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let a = assemble_delete(SCHEMA, &POSTGRES, "t", &pred, Some(100)).unwrap();
        assert_eq!(
            a.template,
            "DELETE FROM \"app_proj\".\"t\" WHERE (tableoid, ctid) IN \
             (SELECT tableoid, ctid FROM \"app_proj\".\"t\" WHERE (\"code\" < $1) LIMIT $2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(0), BindValue::Int(100)]);
    }

    // ── backfill: inline strings (PG path) ───────────────────────────────────

    #[test]
    fn backfill_renders_inline_set_and_filter() {
        let set = BTreeMap::from([(
            "label".to_string(),
            dml_expr(Expr::BinOp {
                op: BinaryOp::Concat,
                lhs: Box::new(Expr::col("code")),
                rhs: Box::new(lit_str("!")),
            }),
        )]);
        let filter = Expr::BinOp {
            op: BinaryOp::Gt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let c = assemble_backfill_clauses(&POSTGRES, "t", &set, Some(&filter)).unwrap();
        assert_eq!(c.set_clause, "\"label\" = (\"code\" || '!')");
        assert_eq!(c.filter.as_deref(), Some("(\"code\" > 0)"));
    }

    /// A string literal in a backfill is `''`-escaped inline (then guard-revalidated
    /// downstream); the quote cannot break out of the literal.
    #[test]
    fn backfill_inline_string_is_quote_escaped() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(lit_str("O'Brien")))]);
        let c = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = 'O''Brien'");
    }

    #[test]
    fn mysql_backfill_string_is_independent_of_backslash_sql_mode() {
        let set = BTreeMap::from([(
            "a".to_string(),
            dml_expr(lit_str("a\\b'; DROP TABLE users; --")),
        )]);
        let c = assemble_backfill_clauses(&MYSQL, "t", &set, None).unwrap();
        assert_eq!(
            c.set_clause,
            "`a` = _utf8mb4 X'615c62273b2044524f50205441424c452075736572733b202d2d'"
        );
        assert!(!c.set_clause.contains("DROP TABLE"));
    }

    #[test]
    fn mysql_backfill_rejects_cross_assignment_dependencies() {
        let swap = BTreeMap::from([
            ("first".to_string(), dml_expr(Expr::col("second"))),
            ("second".to_string(), dml_expr(Expr::col("first"))),
        ]);
        let error = assemble_backfill_clauses(&MYSQL, "t", &swap, None).unwrap_err();
        assert!(
            matches!(
                &error,
                DmlError::MySqlCrossAssignmentDependency {
                    op,
                    column,
                    referenced_column,
                    ..
                } if *op == "backfill" && column == "first" && referenced_column == "second"
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn backfill_empty_set_rejected() {
        let err = assemble_backfill_clauses(&POSTGRES, "t", &BTreeMap::new(), None).unwrap_err();
        assert!(
            matches!(err, DmlError::EmptySet { op: "backfill", .. }),
            "{err:?}"
        );
    }

    // ── splitPart lowering (pinned helper) ───────────────────────────────────

    fn split(col: &str, delim: &str, n: i64) -> Expr {
        Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col(col),
                Expr::lit(IrScalar::Str(delim.into())),
                Expr::lit(IrScalar::Int(n)),
            ],
        }
    }

    /// PG lowers splitPart to the native `split_part(col, 'd', n)` — verbatim.
    #[test]
    fn split_part_pg_native() {
        let set = BTreeMap::from([("first".to_string(), dml_expr(split("name", " ", 1)))]);
        let c = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"first\" = split_part(\"name\", ' ', 1)");
    }

    /// SQLite lowers splitPart to the pinned instr/substr unroll. n=1 is the base
    /// case (no inner walk). The exact string is pinned to the reference exhibit.
    #[test]
    fn split_part_sqlite_n1_unroll() {
        let set = BTreeMap::from([("first".to_string(), dml_expr(split("name", " ", 1)))]);
        let c = assemble_backfill_clauses(&SQLITE, "t", &set, None).unwrap();
        assert_eq!(
            c.set_clause,
            "\"first\" = substr((\"name\" || ' '), 1, instr((\"name\" || ' '), ' ') - 1)"
        );
    }

    /// SQLite n=2 unrolls one boundary walk — pinned to the reference exhibit.
    #[test]
    fn split_part_sqlite_n2_unroll() {
        let set = BTreeMap::from([("last".to_string(), dml_expr(split("name", " ", 2)))]);
        let c = assemble_backfill_clauses(&SQLITE, "t", &set, None).unwrap();
        // cur1 = substr((name||' '), instr((name||' '), ' ') + 1)
        // result = substr(cur1, 1, instr(cur1, ' ') - 1)
        assert_eq!(
            c.set_clause,
            "\"last\" = substr(substr((\"name\" || ' '), instr((\"name\" || ' '), ' ') + 1), \
             1, instr(substr((\"name\" || ' '), instr((\"name\" || ' '), ' ') + 1), ' ') - 1)"
        );
    }

    /// splitPart works in the one-shot (bound) path too — the column arg renders
    /// (binding nested literals); the delim/n are engine-pinned constants, NOT binds.
    #[test]
    fn split_part_one_shot_bound_pg() {
        let set = BTreeMap::from([("first".to_string(), dml_expr(split("name", ",", 1)))]);
        let a = assemble_update(SCHEMA, &POSTGRES, "t", &set, None).unwrap();
        assert_eq!(
            a.template,
            "UPDATE \"app_proj\".\"t\" SET \"first\" = split_part(\"name\", ',', 1)"
        );
        assert!(
            a.binds.is_empty(),
            "delim/n are pinned constants, not binds"
        );
    }

    #[test]
    fn split_part_mysql_delimiter_is_mode_independent_in_both_paths() {
        for (delimiter, encoded) in [("\\", "5c"), ("'", "27")] {
            let set = BTreeMap::from([("part".to_string(), dml_expr(split("name", delimiter, 1)))]);
            let literal = format!("_utf8mb4 X'{encoded}'");
            let expected_expr =
                format!("substring_index(substring_index(`name`, {literal}, 1), {literal}, -1)");

            let backfill = assemble_backfill_clauses(&MYSQL, "t", &set, None).unwrap();
            assert_eq!(backfill.set_clause, format!("`part` = {expected_expr}"));

            let one_shot = assemble_update(SCHEMA, &MYSQL, "t", &set, None).unwrap();
            assert_eq!(
                one_shot.template,
                format!("UPDATE `app_proj`.`t` SET `part` = {expected_expr}")
            );
            assert!(one_shot.binds.is_empty());
            assert!(!one_shot.template.contains("DROP TABLE"));
        }
    }

    /// A single-quote delimiter is `''''`-escaped in the inline literal on both legs.
    #[test]
    fn split_part_quote_delim_escaped_sqlite() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", "'", 1)))]);
        let c = assemble_backfill_clauses(&SQLITE, "t", &set, None).unwrap();
        assert_eq!(
            c.set_clause,
            "\"a\" = substr((\"name\" || ''''), 1, instr((\"name\" || ''''), '''') - 1)"
        );
    }

    /// Renderer fail-closed backstop: an out-of-envelope splitPart (multi-char
    /// delim) that somehow reached the renderer is rejected ON SQLITE, never
    /// mis-built.
    #[test]
    fn split_part_renderer_rejects_out_of_envelope() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ", ", 1)))]);
        let err = assemble_backfill_clauses(&SQLITE, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ",", 9)))]);
        let err = assemble_backfill_clauses(&SQLITE, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// The documented `dialect_scope=PgOnly` escape for an
    /// out-of-envelope `c.fn.splitPart`. The validator ADMITS a
    /// multi-char delimiter and `n > 8` on a Postgres target; the renderer MUST
    /// therefore lower it to native `split_part(col, 'delim', n)` on PG, not
    /// hard-error. This is the missing companion to the load-only grammar test
    /// `out_of_envelope_split_part_pg_loads_sqlite_rejected`.
    #[test]
    fn split_part_out_of_envelope_renders_native_on_pg() {
        // multi-char delimiter — PG's split_part is multi-char-capable.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ", ", 1)))]);
        let c = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", ', ', 1)");

        // n beyond the SQLite unroll bound (9) — PG takes any positive n.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ",", 9)))]);
        let c = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", ',', 9)");

        // and the one-shot (bound) PG path too — delim/n stay pinned constants.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ", ", 1)))]);
        let a = assemble_update(SCHEMA, &POSTGRES, "t", &set, None).unwrap();
        assert_eq!(
            a.template,
            "UPDATE \"app_proj\".\"t\" SET \"a\" = split_part(\"name\", ', ', 1)"
        );
        assert!(
            a.binds.is_empty(),
            "delim/n are pinned constants, not binds"
        );
    }

    /// A non-ASCII (multibyte) delimiter is still PG-renderable (PG splits on the
    /// literal string); the single-ASCII byte gate is a SQLite-only envelope.
    #[test]
    fn split_part_non_ascii_delim_renders_on_pg() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", "→", 2)))]);
        let c = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", '→', 2)");
        // …but rejected on the SQLite leg (out of the byte-wise envelope).
        let err = assemble_backfill_clauses(&SQLITE, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// PG still rejects a structurally-malformed splitPart (non-literal delim, a
    /// non-positive n, a non-string delim) — the PG path widens the ENVELOPE, not
    /// the grammar. These remain unrenderable on both dialects.
    #[test]
    fn split_part_pg_still_rejects_malformed() {
        // n = 0 (not a positive part index) — invalid on PG too.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ",", 0)))]);
        let err = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
        // non-literal delim (a ColRef) — never renderable.
        let bad = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::ColRef {
                    name: "name".into(),
                    table: None,
                },
                Expr::ColRef {
                    name: "name".into(),
                    table: None,
                },
                lit_int(1),
            ],
        };
        let set = BTreeMap::from([("a".to_string(), dml_expr(bad))]);
        let err = assemble_backfill_clauses(&POSTGRES, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }
}
