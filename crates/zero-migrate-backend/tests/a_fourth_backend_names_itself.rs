//! A backend crate that does NOT own `SqlDialect` can still say who it is.
//!
//! # What this test is for
//!
//! `DmlRenderer::dialect` and `SchemaRenderer::dialect` used to return
//! [`SqlDialect`](zero_migrate_ir::dialect::SqlDialect) — a CLOSED enum owned by
//! `zero-migrate-ir`. A vendor crate cannot construct a variant of a closed enum
//! it does not own, so the only body that type-checked in a fourth backend was
//! `todo!()`: the crate compiled and then panicked the first time anything asked
//! it who it was. That is not a registry problem — a stub fourth backend already
//! registers and is REACHED through the real registry — it is a signature
//! problem, and it is the one thing that stopped a vendor crate from lowering a
//! migration.
//!
//! Both methods return [`DialectId`] now. This file is the proof: `DuckDb` below
//! is a complete outsider. It is declared in a test binary, `zero-migrate-ir`
//! has never heard of it, `SHIPPING_DESCRIPTORS` does not list it, and the whole
//! file contains no mention of `SqlDialect` at all — the assertion at the bottom
//! of the module enforces that by reading this source file back.
//!
//! The spelling bodies are deliberately thin. The claim under test is IDENTITY,
//! not fidelity: a fourth backend's `dialect()` has a real body, and everything
//! that asks a renderer who it is gets an honest answer instead of a panic.

use zero_migrate_backend::dml::DmlError;
use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_backend::schema::SchemaRenderer;
use zero_migrate_backend::snapshot::ColumnSnapshot;
use zero_migrate_backend::step::BindValue;
use zero_migrate_backend::vendor::VendorStatement;
use zero_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::expr::{CastTarget, ExtractField, ScalarFn};
use zero_migrate_ir::ir::{IrScalar, Op, TableRef};

/// The outsider's own identity, declared at item scope in a crate that owns
/// neither `SqlDialect` nor the shipping registry. `DialectId::new` is `const`,
/// which is what makes this line possible at all.
const DUCKDB: DialectId = DialectId::new("duckdb");

/// The outsider's own descriptor — the ONE thing it declares about itself, and
/// the value both its identity and its capability answers are read off. Every
/// item on the right-hand side is `const`, so an out-of-tree crate writes this at
/// item scope exactly as it appears here.
static DUCKDB_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: DUCKDB,
    display_name: "DuckDB",
    capabilities: CapabilitySet::empty()
        .with(Capability::TableLevelForeignKey)
        .with(Capability::TableLevelUnique)
        .with(Capability::CreateOrReplaceView)
        .with(Capability::Sequence),
    limits: Limits {
        identifier: IdentifierLimit::Unbounded,
    },
};

#[derive(Debug)]
struct DuckDbDmlRenderer;

#[derive(Debug)]
struct DuckDbSchemaRenderer;

impl DmlRenderer for DuckDbDmlRenderer {
    fn descriptor(&self) -> &'static BackendDescriptor {
        &DUCKDB_DESCRIPTOR
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError> {
        Ok(format!(
            "{}.{}",
            self.quote_ident(project_schema),
            self.quote_ident(table)
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "VARCHAR",
            CastTarget::Int => "BIGINT",
            CastTarget::Real => "DOUBLE",
            CastTarget::Boolean => "BOOLEAN",
            CastTarget::Bytes => "BLOB",
            CastTarget::Uuid => "UUID",
        }
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn inline_string_literal(&self, s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        d.to_string()
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        let mut out = String::from("'");
        for b in bytes {
            out.push_str(&format!("\\x{b:02X}"));
        }
        out.push_str("'::BLOB");
        out
    }

    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        push(BindValue::Bytes(bytes.to_vec()))
    }

    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError> {
        let rendered: Vec<String> = elems.iter().map(|e| format!("{e:?}")).collect();
        let op = if negated { "NOT IN" } else { "IN" };
        Ok(format!("{expr} {op} ({})", rendered.join(joiner)))
    }

    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError> {
        Ok(format!(
            "regexp_matches({expr}, {})",
            self.inline_string_literal(pattern)
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> String {
        format!("date_part('{field:?}', {expr})")
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        format!("({l} || {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} IS DISTINCT FROM {r})")
    }

    fn render_scalar_fn_override(&self, _f: ScalarFn, _args: &[String]) -> Option<String> {
        None
    }

    fn render_is_true(&self, operand: &str) -> String {
        format!("({operand} IS TRUE)")
    }

    fn render_is_false(&self, operand: &str) -> String {
        format!("({operand} IS FALSE)")
    }

    fn render_concat_ws(&self, rendered: &[String]) -> String {
        format!("concat_ws({})", rendered.join(", "))
    }

    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError> {
        Ok(format!(
            "str_split({col_sql}, {})[{n}]",
            self.inline_string_literal(delim)
        ))
    }

    fn synth_now(&self) -> String {
        "now()".to_string()
    }

    fn uuid_v4(&self) -> String {
        "uuid()".to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "duckdb has no uuidv7 generator".to_string(),
        ))
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        if materialized {
            return Err(IrLowerError::DmlAssemble(DmlError::UnrenderableExpr(
                "duckdb has no materialized views".to_string(),
            )));
        }
        Ok(if replace {
            "CREATE OR REPLACE VIEW".to_string()
        } else {
            "CREATE VIEW".to_string()
        })
    }

    fn view_replace_prelude(&self, _qname: &str, _replace: bool) -> Vec<String> {
        Vec::new()
    }

    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(format!(
            "{}.{}",
            self.quote_ident(eff_schema),
            self.quote_ident(name)
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let schema = table.schema.as_deref().unwrap_or(eff_schema);
        Ok(format!(
            "{}.{}",
            self.quote_ident(schema),
            self.quote_ident(&table.name)
        ))
    }

    fn render_trigger_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<Vec<VendorStatement>, IrLowerError> {
        Err(IrLowerError::DmlAssemble(DmlError::UnrenderableExpr(
            "duckdb has no triggers".to_string(),
        )))
    }

    /// The newcomer WRITES ITS OWN REFUSAL, and that is the point of the method
    /// having no default body.
    ///
    /// `render_vendor_op` covers sixteen op kinds that are PostgreSQL-only. A
    /// default body would have let this backend inherit somebody else's answer
    /// silently; a required method makes the omission `E0046` in the newcomer's
    /// own crate, so the only way to compile is to state a position. DuckDb has no
    /// vendor-op surface, so it refuses, naming ITSELF — exactly as the shipping
    /// SQLite and MySQL renderers do.
    fn render_vendor_op(
        &self,
        _op: &zero_migrate_ir::ir::Op,
        _eff_schema: &str,
    ) -> Result<
        Vec<zero_migrate_backend::vendor::VendorStatement>,
        zero_migrate_backend::vendor::VendorError,
    > {
        Err(zero_migrate_backend::vendor::VendorError::VendorOpsUnsupported(DUCKDB))
    }
}

impl SchemaRenderer for DuckDbSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DUCKDB
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("\"{app_id}\".\"{target}\"")
    }

    fn column_type(&self, column: &ColumnSnapshot, _inline_pk: bool) -> String {
        match column.data_type.as_str() {
            "double precision" => "DOUBLE".to_string(),
            "boolean" => "BOOLEAN".to_string(),
            _ => "VARCHAR".to_string(),
        }
    }

    /// This stub deliberately supports no collation spelling. The required method
    /// makes that refusal-to-transform explicit in the outsider's own crate.
    fn pin_collation(&self, rendered: &str, _case_sensitive: Option<bool>) -> String {
        rendered.to_string()
    }

    /// With no pin of its own, DuckDB has no suffix of its own to strip.
    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str {
        rendered
    }

    fn json_object_default(&self) -> String {
        "'{}'".to_string()
    }

    fn json_array_default(&self) -> String {
        "'[]'".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "now()"
    }

    fn column_comment_statements(
        &self,
        _app_id: &str,
        _collection: &str,
        _schema: &serde_json::Value,
    ) -> Vec<String> {
        Vec::new()
    }
}

/// The claim, stated as a value: an outsider's `dialect()` returns its OWN id.
///
/// Before the signature change the only body that type-checked here was
/// `todo!()`, so this call panicked. It returns a value now, and the value is
/// the one the outsider declared — not one of the three the core enum knows.
#[test]
fn a_fourth_backend_answers_dialect_with_its_own_id() {
    let dml: &dyn DmlRenderer = &DuckDbDmlRenderer;
    let schema: &dyn SchemaRenderer = &DuckDbSchemaRenderer;

    assert_eq!(dml.dialect(), DUCKDB);
    assert_eq!(schema.dialect(), DUCKDB);
    assert_eq!(dml.dialect().as_str(), "duckdb");
    assert!(dml.dialect().is_well_formed());
    assert_eq!(
        schema.pin_collation("VARCHAR", Some(false)),
        "VARCHAR",
        "the outsider writes its own pass-through instead of inheriting one"
    );
    assert_eq!(schema.strip_collation("VARCHAR"), "VARCHAR");

    // Capabilities come off the outsider's OWN descriptor, so the answers are the
    // ones it declared — not the "no to everything" a core-owned id->capability
    // table would have to give a name it does not recognise.
    assert!(dml.supports(Capability::CreateOrReplaceView));
    assert!(!dml.supports(Capability::PostgresVendorPrimitives));
    assert!(!dml.supports(Capability::MaterializedView));

    // And it is a genuine outsider: none of the three shipping ids is this one.
    for shipped in zero_migrate_ir::backend::SHIPPING_DESCRIPTORS {
        assert_ne!(
            shipped.id, DUCKDB,
            "the stub is supposed to be a backend the core does not ship"
        );
    }
}

/// The stub is only a proof if it never touches the closed enum.
///
/// A test that demonstrated a fourth backend by NAMING `SqlDialect` somewhere
/// would be demonstrating the opposite thing. This reads its own source back and
/// refuses the mention, so the proof cannot rot into one by a later edit.
///
/// The needle is assembled from two halves on purpose. Spelled whole, the
/// detector's own line is the first thing it finds and the test fails on itself —
/// which it did, on the first run. A scanner that matches its own source is the
/// standard failure of this shape, and the fix has to be in the LITERAL rather
/// than in an exclusion rule, because any "skip line N" carve-out would also skip
/// a real offender that later lands on that line.
#[test]
fn the_stub_never_names_the_closed_enum() {
    let source = include_str!("a_fourth_backend_names_itself.rs");
    let needle = concat!("Sql", "Dialect");
    let offenders: Vec<(usize, &str)> = source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .map(|(i, line)| (i + 1, line.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "the fourth-backend stub must name no closed dialect enum, but it does: {offenders:#?}"
    );
}
