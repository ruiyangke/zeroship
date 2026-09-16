//! The LINE-1 CONTRACT - what a backend crate's security guard must satisfy.
//!
//! This is the third per-vendor trait in this crate, alongside
//! [`crate::renderer::DmlRenderer`] ("how does this vendor spell DML") and
//! [`crate::schema::SchemaRenderer`] ("how does this vendor spell DDL").
//! [`MigrationGuard`] answers the third question: **"what does this vendor refuse
//! to run"**.
//!
//! # Why the contract is HERE and not in the engine
//!
//! The engine (`zero-migrate`) depends on all three vendor crates, and each vendor
//! crate depends on this one. Declaring [`MigrationGuard`] in the engine would force
//! every vendor to depend on the engine in order to implement it, and the engine
//! already depends on every vendor - a crate cycle Cargo refuses. The arrow only
//! works one way: the contract sits BELOW every vendor, the vendors implement it,
//! and the engine composes them. That is the same reason
//! [`crate::registry::BackendVendor`] lives here.
//!
//! # What lives here
//!
//! The SEAM: [`GuardConfig`], [`GuardError`], [`GuardOutcome`],
//! [`ParseError`] and [`IrDataSecurityError`], plus [`crate::advisory`]. Every
//! vendor's guard is configured by the same unforgeable [`EffectivePolicy`] and
//! reports in the same vocabulary, so the seam is genuinely neutral and belongs below
//! every vendor.
//!
//! [`check_ir_data_security_policy`] lives here too, and placing it neutrally
//! had one real obstacle, worth recording because the shape recurs:
//!
//! - It is the ONLY `destructive_ops = forbid` enforcement SQLite and MySQL have,
//!   because the descriptor guard those two dialects run is constructed without
//!   the policy and cannot read the knob at all.
//! - Yet it reached `pg_query::parse` for `Op::Raw` islands, because a raw island's
//!   net table state is not enumerable without a parser.
//!
//! So the function enforcing SQLite's and MySQL's posture needed PostgreSQL's parser.
//! Filing it under `zeroship-migrate-postgres` would have put two dialects' only
//! data-security enforcement in a third dialect's crate; putting it here as it stood
//! would have pushed `libpg_query` beneath `zeroship-migrate-sqlite` and
//! `zeroship-migrate-mysql`, which build without it.
//!
//! Neither was necessary, because the parser was answering ONE question. The walk
//! now asks it through [`MigrationGuard::raw_island_escapes_rls_net_state`]: a vendor
//! with a raw door answers from its parser, and a descriptor-only vendor answers "I
//! have no raw door" without anyone parsing anything. Everything else the walk
//! decides is read off a structured [`Op`], which needs no grammar. That is the
//! general move - when neutral code seems to need a vendor's tool, find the single
//! question it is using the tool to answer and make THAT the vendor's method.
//!
//! # A vendor that ships no guard does not compile
//!
//! [`MigrationGuard::check`] has NO default body, and
//! [`crate::registry::BackendVendor`]'s `guard` field is not optional. A new backend
//! that supplies no guard fails to construct its `BackendVendor` (E0063, missing
//! field) at its OWN definition site, naming the vendor. It cannot silently inherit a
//! trusting one. Trusting the input is still allowed, but only by WRITING
//! `Ok(GuardOutcome::default())`, which is a decision a reviewer can see in the diff.
//!
//! The required field is what makes that hold: there is no closed dialect match
//! handing several vendors one shared guard type. A closed match would be
//! exhaustive, so a fourth dialect breaks it - but a `_ =>` arm added in haste
//! would grant every future backend the trusting path silently. The required
//! field removes the arm the mistake could be made in. `BackendVendor` therefore
//! derives no `Default`, has no `Default` impl, is
//! not `#[non_exhaustive]`, and its `guard` field is not an `Option` - each of those
//! would reintroduce exactly the silent grant of trust this removes.

use std::collections::{BTreeMap, BTreeSet};

use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::ir::{MigrationIr, Op};
use zeroship_migrate_ir::migration::MigrationFlags;
use zeroship_migrate_ir::policy::DestructiveOps;
use zeroship_migrate_ir::policy::SchemaScope;
use zeroship_migrate_ir::policy_registry;
use zeroship_migrate_policy::{
    normalize_object_name, EffectivePolicy, GrantRegion, KnobKey, KnobValue, ObjectModel,
    ObjectName, ShapeElement,
};

use crate::advisory::Advisory;

/// Error parsing SQL for classification.
///
/// Neutral because [`GuardError::Parse`] carries it and [`GuardError`] is the
/// vocabulary every vendor's guard reports in. The PARSER that raises it is not
/// neutral - today only `zeroship-migrate-postgres` has one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The vendor's parser rejected the SQL (syntax error, etc.).
    #[error("failed to parse SQL: {0}")]
    Syntax(String),
}

/// Stable data-security policy rule ids. These are policy decisions layered on
/// the guard, not deny-list parser rules.
pub mod data_security_rule {
    /// `data_security.destructive_ops = "forbid"` denied a destructive operation.
    pub const DESTRUCTIVE_OPS_FORBID: &str = "DATA_SECURITY_DESTRUCTIVE_OPS_FORBID";
    /// `data_security.destructive_ops = "forbid"` denied an unclassified operation.
    pub const UNCLASSIFIED_OP_DENIED_UNDER_FORBID: &str =
        "DATA_SECURITY_UNCLASSIFIED_OP_DENIED_UNDER_FORBID";
    /// `data_security.require_rls = true` denied a create-table without RLS enable.
    pub const REQUIRE_RLS: &str = "DATA_SECURITY_REQUIRE_RLS";
}

/// Stable NAMESPACE-authority policy rule ids (II.2.5 / II.2.6). These are the
/// conservative-deny rules the policy redesign introduces on top of the deny-list:
/// raw-SQL create/DDL classification, per-op creation-gating, and injected-shape
/// immutability. Each fails closed with the design's named error code.
pub mod namespace_rule {
    /// II.2.5 - a raw create (`CREATE TABLE` / CTAS / `SELECT INTO` / `LIKE` /
    /// `PARTITION OF` / `CREATE TABLE AS EXECUTE` / `INHERITS`) targets an object an
    /// `inject` rule covers and its own text does not carry the injected shape.
    /// Injection cannot rewrite raw text, so a create that does not already declare
    /// every injected column and exactly the pinned primary key would land a table
    /// the inject rule was supposed to shape.
    pub const RAW_CREATE_IN_INJECT_SCOPE: &str = "RawCreateInInjectScope";
    /// II.2.6a - a create (`CREATE TABLE`, structured or classified-raw) is not
    /// covered by a `schema.create_table` grant (default-deny namespace anchor).
    pub const CREATE_TABLE_NOT_GRANTED: &str = "CreateTableNotGranted";
    /// II.2.6a - a `CREATE SCHEMA` (structured or classified-raw) is not covered by
    /// a `schema.create_schema` grant.
    pub const CREATE_SCHEMA_NOT_GRANTED: &str = "CreateSchemaNotGranted";
    /// II.2.5 - a raw rename / `SET SCHEMA` moves a table INTO an inject scope; the
    /// engine cannot re-inject over raw text, so the move is denied.
    pub const RAW_RENAME_INTO_INJECT_SCOPE: &str = "RawRenameIntoInjectScope";
    /// II.2.6a - a rename/move into a scope is not covered by a `schema.rename`
    /// grant at the target.
    pub const RENAME_INTO_NOT_GRANTED: &str = "RenameIntoNotGranted";
    /// II.2.5 - an unqualified object reference under a non-Top `sql.raw` grant is
    /// unattributable (no live search_path to resolve it) -> Top-only -> deny.
    pub const UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL: &str = "UnqualifiedNameUnderScopedRawSql";
    /// II.2.5 - `SET search_path` (or equivalent) under a non-Top `sql.raw` grant
    /// mutates the very name-resolution context attribution depends on -> refused.
    pub const SEARCH_PATH_UNDER_SCOPED_RAW_SQL: &str = "SearchPathUnderScopedRawSql";
    /// II.2.5 - an opaque-body construct (`CREATE FUNCTION`/`PROCEDURE`/`TRIGGER`/
    /// `DO`) under a non-Top `sql.raw` grant defeats statement-level attribution.
    pub const OPAQUE_BODY_UNDER_SCOPED_RAW_SQL: &str = "OpaqueBodyUnderScopedRawSql";
    /// II.2.5 - a raw statement the parser cannot classify into exactly one shape,
    /// or whose target is dynamic/unqualified, is unattributable under a non-Top grant.
    pub const UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL: &str = "UnattributableRawUnderScopedRawSql";
    /// II.2.6b - an `ALTER`/`DROP COLUMN`/`RENAME` touching a column the covering
    /// inject rule contributes, without an explicit `schema.alter_injected` grant.
    pub const INJECTED_SHAPE_IMMUTABLE: &str = "InjectedShapeImmutable";
    /// II.2.6b - an index-mutating op on an injected index.
    pub const INJECTED_INDEX_IMMUTABLE: &str = "InjectedIndexImmutable";
    /// II.2.6b - a PK-replacing/dropping op on a table whose PK a covering inject
    /// rule pins.
    pub const INJECTED_PRIMARY_KEY_IMMUTABLE: &str = "InjectedPrimaryKeyImmutable";
    /// II.2.6b (H3) - a rename-into where a name-matching element diverges
    /// structurally from the injected shape (type/nullability/default/key/PK-columns).
    pub const INJECTED_SHAPE_CONFORMANCE_MISMATCH: &str = "InjectedShapeConformanceMismatch";
}

/// Per-guard configuration.
///
/// All fields are private. A caller must supply an explicitly composed
/// [`EffectivePolicy`] through [`GuardConfig::from_policy`]. The guard never selects
/// or fabricates a policy from a named posture.
///
/// There is no belt-off posture. The static parse-time guard belt - the deny-list,
/// cross-schema confinement and body walks - runs for every config this type can
/// build. The engine used to carry a root/host-set mode that skipped it for a
/// dbmate-like Trusted operator, and removing that mode is what makes the belt
/// unconditional here.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// PRIVATE. The target SQL dialect this guard config is for.
    ///
    /// - `postgres` - the `libpg_query` line-1 guard runs
    ///   (`SqlGuard::check` parses + deny-walks the SQL).
    /// - every other id - a vendor-owned guard path. An untrusted raw SQL string
    ///   presented to PostgreSQL's `SqlGuard::check` is refused.
    dialect: DialectId,
    /// The target schema selected by the trusted host, independent of policy grants.
    project_schema: String,
    /// PRIVATE. The unforgeable [`EffectivePolicy`] - the SINGLE source the guard's
    /// every composable decision queries. The capability gate asks `grants(key,
    /// object)` for the statement's builtin knob key; references outside the target ask
    /// `grants(schema.cross_schema, schema)`; the data-security obligations read
    /// `obligations`/`grants` on the safety.* knobs. There is no separate posture/scope
    /// state for the composable knobs - the policy IS the posture.
    effective: EffectivePolicy,
}

impl GuardConfig {
    /// Construct a guard from the host-selected target, composed policy, and dialect. The
    /// full belt runs for every config: the effective policy is the SINGLE source for
    /// injection and every composable guard decision, and there is no separate posture
    /// that turns the belt off.
    #[must_use]
    pub fn from_policy(
        effective: EffectivePolicy,
        dialect: DialectId,
        project_schema: impl Into<String>,
    ) -> Self {
        Self {
            dialect,
            effective,
            project_schema: project_schema.into(),
        }
    }

    /// The host-selected schema unqualified migration objects resolve against.
    #[must_use]
    pub fn project_schema(&self) -> &str {
        &self.project_schema
    }

    /// Replace the composed policy while preserving the host-selected target and dialect.
    #[must_use]
    pub fn with_effective_policy(mut self, effective: EffectivePolicy) -> Self {
        self.effective = effective;
        self
    }

    /// Borrow the composed [`EffectivePolicy`] this config decides against.
    ///
    /// This exists so the `effective` FIELD can stay private now that a vendor's
    /// guard lives one crate above the config it reads. It is a read-only borrow of a
    /// policy the caller already holds - it grants nothing that
    /// [`GuardConfig::with_effective_policy`] does not already allow, and it is what
    /// keeps the `compile_fail` struct-literal boundary below intact: an external
    /// crate still cannot name its private fields.
    #[must_use]
    pub const fn effective(&self) -> &EffectivePolicy {
        &self.effective
    }

    /// Select a target dialect without changing the caller-composed policy.
    #[must_use]
    pub fn for_dialect(mut self, dialect: DialectId) -> Self {
        self.dialect = dialect;
        self
    }

    /// The target SQL dialect this guard config vets.
    #[must_use]
    pub const fn dialect(&self) -> &DialectId {
        &self.dialect
    }

    /// The target schema together with foreign schemas admitted by policy.
    /// A cross-schema grant never selects or replaces the target schema.
    #[must_use]
    pub fn schema_scope(&self) -> Option<SchemaScope> {
        let key = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok()?;
        if matches!(self.effective.grant_region(&key), GrantRegion::Top) {
            return Some(SchemaScope::Unconfined);
        }
        let literal_schemas = self.effective.grant_literal_schema_includes(&key);
        if matches!(self.effective.grant_region(&key), GrantRegion::Scoped)
            && literal_schemas.is_none()
        {
            return Some(SchemaScope::Policy {
                project_schema: self.project_schema.clone(),
                effective: Box::new(self.effective.clone()),
            });
        }
        let mut schemas = literal_schemas.unwrap_or_default();
        schemas.retain(|schema| self.permits_schema(schema));
        if !self.project_schema.is_empty() {
            schemas.push(self.project_schema.clone());
        }
        schemas.sort();
        schemas.dedup();
        Some(match schemas.as_slice() {
            [one] => SchemaScope::Single(one.clone()),
            _ => SchemaScope::Allowlist(schemas),
        })
    }

    /// Data-security destructive-op posture carried into the guard (the
    /// `safety.destructive_ops` grant).
    #[must_use]
    pub fn destructive_ops(&self) -> DestructiveOps {
        self.effective_destructive_ops()
    }

    // --- PDP decision-query helpers ------------------------------------------
    // The guard's capability + data-security gate asks these instead of reading a
    // raw `VendorCapabilities` bit / `require_rls` / `destructive_ops` field.
    // Composable scope decisions live inside the `EffectivePolicy`; the guard passes
    // a concrete object and reads back a value. The host selects the target schema.

    /// Does the effective policy GRANT the whole-DB (Global) capability `key` at
    /// `object`? A Global Bool grant resolves the same at every object; we pass a
    /// stable global witness. Reproduces `caps.grants(cap)`: absent grant => the
    /// knob default (`false`, deny).
    pub fn grants_global_bool(&self, key: &str) -> bool {
        self.grants_bool_at(key, &self.global_witness_for(key))
    }

    /// The witness object for a Global-model read of `key`, checked against the
    /// registry rather than asserted in prose: a knob whose `ObjectModel` is not
    /// `Global` resolves differently at different objects, so reading it at ONE
    /// witness erases the rule's scope.
    ///
    /// `schema.create_schema`, `access.policy` and `access.rls` are object-scoped;
    /// they go through [`Self::grants_object_bool`] with the object their statement
    /// names.
    pub fn global_witness_for(&self, key: &str) -> ObjectName {
        debug_assert!(
            self.knob_object_model(key) == Some(ObjectModel::Global),
            "{key} is not a Global-model knob: a single witness erases its scope"
        );
        global_witness()
    }

    /// Does the effective policy grant the OBJECT-SCOPED Bool `key` for a statement
    /// targeting `object`?
    ///
    /// `None` is a statement whose target the guard cannot name: an unqualified
    /// relation without a host-selected target, a `CREATE SCHEMA
    /// AUTHORIZATION` form carrying no schema name. Such a target is not provably
    /// inside any narrower scope, so only a whole-universe grant reaches it.
    pub fn grants_object_bool(&self, key: &str, object: Option<&ObjectName>) -> bool {
        match object {
            Some(object) => self.grants_bool_at(key, object),
            None => self.grants_bool_everywhere(key),
        }
    }

    /// Does the effective policy grant Bool `key` at EVERY object - a whole-universe
    /// ([`GrantRegion::Top`]) rule? Such a grant resolves the same everywhere, so one
    /// witness decides it; any narrower region answers `false`, because the caller
    /// holds no object to test a narrower rule against.
    pub fn grants_bool_everywhere(&self, key: &str) -> bool {
        let Some(k) = KnobKey::parse(key).ok() else {
            return false;
        };
        matches!(self.effective.grant_region(&k), GrantRegion::Top)
            && self.grants_bool_at(key, &global_witness())
    }

    /// The registered [`ObjectModel`] of `key`, or `None` for a key this policy's
    /// registry does not define.
    pub fn knob_object_model(&self, key: &str) -> Option<ObjectModel> {
        let k = KnobKey::parse(key).ok()?;
        Some(self.effective.registry().get(&k)?.object_model)
    }

    /// Does the effective policy grant Bool `key` at the concrete `object`? For a
    /// PerSchema/PerTable knob the object attributes the grant. Non-Bool / unknown
    /// keys fail closed to `false`.
    pub fn grants_bool_at(&self, key: &str, object: &ObjectName) -> bool {
        let Some(k) = KnobKey::parse(key).ok() else {
            return false;
        };
        matches!(
            self.effective.grants(&k, object),
            Some(KnobValue::Bool(true))
        )
    }

    /// The extension names the effective policy permits - the `code.extension` StrSet
    /// grant value at the global witness. Empty when no grant covers it
    /// (deny-by-default, so no `CREATE` and no `DROP`). The guard's hard deny still
    /// overrides this regardless of what a charter lists.
    ///
    /// # There is deliberately no scalar "holds the extension capability" beside this
    ///
    /// `code.extension` is the one capability knob whose value is a SET OF NAMES
    /// rather than a Bool, so a scalar "is the list non-empty" check throws away
    /// the entire grant: a charter permitting one extension would answer yes for
    /// every other extension in the database. Any caller deciding a statement has
    /// a name in hand and must match it against this list; a caller that has no
    /// name has no question this can answer.
    pub fn granted_extension_allowlist(&self) -> Vec<String> {
        let Some(k) = KnobKey::parse(policy_registry::KEY_CODE_EXTENSION).ok() else {
            return Vec::new();
        };
        let witness = self.global_witness_for(policy_registry::KEY_CODE_EXTENSION);
        match self.effective.grants(&k, &witness) {
            Some(KnobValue::StrSet(names)) => names,
            _ => Vec::new(),
        }
    }

    /// True iff the effective policy obligates RLS on `object`: a covering
    /// `safety.require_rls` Require rule.
    ///
    /// The knob is registered `ObjectModel::PerTable`, so this takes the object it is
    /// deciding about. There is deliberately no scalar form: a charter may obligate
    /// RLS on one table, one schema, or everything, and a caller holding no object
    /// has no question this can answer.
    #[must_use]
    pub fn requires_rls_at(&self, object: &ObjectName) -> bool {
        let Some(want) = KnobKey::parse(policy_registry::KEY_SAFETY_REQUIRE_RLS).ok() else {
            return false;
        };
        self.effective
            .obligations(object)
            .iter()
            .any(|(k, v)| *k == want && matches!(v, KnobValue::Bool(true)))
    }

    /// [`Self::requires_rls_at`] for a caller holding a schema and a table name
    /// rather than a built [`ObjectName`].
    ///
    /// The object is built THROUGH [`normalize_object_name`], the same way
    /// `zeroship_migrate_ir::policy_approval` builds its own: the composer's scope matcher
    /// PG-folds both sides, so raw table bytes would let a table spelled `"Users"` slip
    /// past a scope of `app.users`.
    ///
    /// A pair no `ObjectName` can be built from - an empty schema, say - is not
    /// provably outside the obligation, so it falls back closed onto the weaker
    /// question `require_rls_authored_anywhere` asks: is the obligation authored at
    /// ANY scope? That refuses such a table under an RLS charter and leaves a charter
    /// that never mentions RLS untouched.
    #[must_use]
    pub fn requires_rls_at_table(&self, schema: &str, table: &str) -> bool {
        match normalize_object_name(&format!("{schema}.{table}")) {
            Some(object) => self.requires_rls_at(&object),
            None => self.require_rls_authored_anywhere(),
        }
    }

    /// Is a `safety.require_rls = true` obligation authored ANYWHERE in the effective
    /// policy, at any scope?
    ///
    /// This is the fail-closed side of [`Self::requires_rls_at`], and it is only ever
    /// correct where there is no object to resolve at: a table the guard cannot
    /// qualify, or a raw statement naming no relation. Such an input is not provably
    /// outside the obligation, so it is refused, but only when an obligation exists
    /// to refuse it against, otherwise a charter that never mentions RLS would start
    /// rejecting migrations.
    pub fn require_rls_authored_anywhere(&self) -> bool {
        let Some(want) = KnobKey::parse(policy_registry::KEY_SAFETY_REQUIRE_RLS).ok() else {
            return false;
        };
        self.effective
            .obligation_values_anywhere(&want)
            .iter()
            .any(|v| matches!(v, KnobValue::Bool(true)))
    }

    /// The effective destructive-op posture - the `safety.destructive_ops` OrderedEnum
    /// grant value at the global witness, mapped back onto [`DestructiveOps`]. The
    /// deny-by-default value is `forbid`; an absent grant resolves to the knob
    /// default (`forbid`). This drives the destructive gating.
    pub fn effective_destructive_ops(&self) -> DestructiveOps {
        let Some(k) = KnobKey::parse(policy_registry::KEY_SAFETY_DESTRUCTIVE_OPS).ok() else {
            return DestructiveOps::Forbid;
        };
        let witness = self.global_witness_for(policy_registry::KEY_SAFETY_DESTRUCTIVE_OPS);
        match self.effective.grants(&k, &witness) {
            Some(KnobValue::Str(s)) => match s.as_str() {
                "allow" => DestructiveOps::Allow,
                "warn" => DestructiveOps::Warn,
                _ => DestructiveOps::Forbid,
            },
            _ => DestructiveOps::Forbid,
        }
    }

    // --- namespace-authority decision queries: pinned schema, creation grants,
    // covering inject shapes, and cross-schema admission ----------------------

    /// The host-selected schema for an unqualified relation. A malformed empty
    /// target cannot attribute a relation and fails closed.
    pub fn pinned_schema(&self) -> Option<String> {
        (!self.project_schema.is_empty()).then(|| self.project_schema.clone())
    }

    /// True iff the effective policy grants Bool `key` (default-deny) at the concrete
    /// normalized `object`. Fails closed (`false`) on an unknown key or non-Bool value.
    pub fn grants_namespace_bool(&self, key: &str, object: &ObjectName) -> bool {
        self.grants_bool_at(key, object)
    }

    /// The [`GrantRegion`] of `sql.raw` - the whole-universe / Scoped / Ungranted posture the
    /// scoped-raw-SQL rules (II.2.5) turn on.
    pub fn raw_sql_region(&self) -> GrantRegion {
        match KnobKey::parse(policy_registry::KEY_SQL_RAW) {
            Ok(k) => self.effective.grant_region(&k),
            Err(_) => GrantRegion::Ungranted,
        }
    }

    /// Does ANY covering `inject` rule contribute to `object`? A raw rename/move
    /// into an inject scope is denied on this answer alone: unlike a create, the
    /// moved table's shape is nowhere in the statement text, so nothing can prove
    /// the injection was honoured (II.2.5).
    pub fn injects_cover(&self, object: &ObjectName) -> bool {
        !self.effective.injects_for(object).is_empty()
    }

    /// What every covering `inject` rule demands of a table created at `object`,
    /// with each policy-authored name folded to the PostgreSQL identifier bytes it
    /// denotes. Drives the raw-create conformance check; empty when no inject covers
    /// `object`.
    pub fn covering_inject_shapes(&self, object: &ObjectName) -> Vec<InjectedCreateShape> {
        self.effective
            .injects_for(object)
            .into_iter()
            .map(|spec| InjectedCreateShape {
                columns: spec
                    .columns
                    .iter()
                    .map(|column| fold_identifier(&column.name))
                    .collect(),
                primary_key: spec.primary_key.as_ref().map(|keys| {
                    keys.iter()
                        .map(|key| fold_identifier(key))
                        .collect::<Vec<_>>()
                }),
            })
            .collect()
    }

    /// Is `element` on `object` an injected shape element (II.2.6b, name-match-at-op-time)?
    pub fn is_injected_shape(&self, object: &ObjectName, element: &ShapeElement) -> bool {
        self.effective.is_injected_shape(object, element)
    }

    /// Whether a schema reference stays within the host-selected target or is
    /// explicitly authorized by a foreign-schema grant. This does not grant any
    /// operation capability such as table creation or rename.
    pub fn permits_schema(&self, schema: &str) -> bool {
        if schema.is_empty() || schema.contains('\0') {
            return false;
        }
        if schema == self.project_schema {
            return true;
        }
        let Some(k) = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok() else {
            return false;
        };
        let Some(object) = normalize_object_name(schema) else {
            return false;
        };
        matches!(
            self.effective.grants(&k, &object),
            Some(KnobValue::Bool(true))
        )
    }
}

/// The EXTERNAL trust boundary, pinned as `compile_fail` doctests. A doctest
/// is compiled as a SEPARATE crate that `use`s `zeroship_migrate_backend`, so it
/// exercises exactly the boundary an external consumer of this crate sits behind.
///
/// KEEP THE FIELD LISTS BELOW EXACT. A `compile_fail` doctest passes when the code
/// fails to compile for ANY reason, so a literal naming a field that no longer
/// exists passes on the typo and stops testing privacy at all. When a field is
/// added or renamed, update these and re-check them the only way that means
/// anything - make the fields `pub`, confirm the doctests FAIL, then put the
/// visibility back.
///
/// (1) An external crate cannot write a `GuardConfig { .. }` struct literal - the
/// fields (`dialect`, `effective`, `project_schema`) are private, so a privileged
/// profile can never be forged by a literal (the `EffectivePolicy` is itself
/// unforgeable). This MUST fail to compile:
///
/// ```compile_fail
/// use zeroship_migrate_backend::guard::GuardConfig;
/// let _ = GuardConfig {
///     dialect: zeroship_migrate_ir::dialect::DialectId::new("postgres"),
///     effective: unimplemented!(),
///     project_schema: "app".into(),
/// };
/// ```
///
/// (2) The only unforgeable input to a privileged `GuardConfig` is a composed
/// `EffectivePolicy` - an external crate cannot construct one by a literal (its
/// fields are private and it has no public constructor other than `deny_all`). This
/// MUST fail to compile:
///
/// ```compile_fail
/// let _ = zeroship_migrate_policy::EffectivePolicy {
///     registry: unimplemented!(),
///     layers: unimplemented!(),
/// };
/// ```
///
/// There is no policy-free constructor or default. Every usable config therefore
/// starts with a caller-composed `EffectivePolicy`.
#[cfg(doctest)]
struct ExternalTrustBoundaryCompileFail;

/// A guard rejection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    /// A hard-denied dangerous construct (RCE / priv-esc / file / network).
    #[error("denied by rule '{rule}': {statement}")]
    Denied {
        /// The stable rule id (see `denylist::rule`).
        rule: &'static str,
        /// The offending statement text.
        statement: String,
    },
    /// A reference to a schema outside the project schema (cross-tenant).
    #[error("cross-schema access to '{schema}' denied: {statement}")]
    CrossSchema {
        /// The foreign schema that was referenced.
        schema: String,
        /// The offending statement text.
        statement: String,
    },
    /// A data-security profile knob denied the migration.
    #[error("data_security policy '{rule}' denied: {statement}")]
    DataSecurityPolicy {
        /// The stable data-security rule id.
        rule: &'static str,
        /// The offending statement or IR op.
        statement: String,
    },
    /// A NAMESPACE-authority rule (II.2.5 raw-SQL classification / II.2.6
    /// creation-gating + injected-shape immutability) denied the migration. Each is
    /// a conservative-deny with the design's named error code (see `namespace_rule`).
    #[error("namespace policy '{rule}' denied: {statement}")]
    NamespacePolicy {
        /// The stable namespace rule id (see `namespace_rule`).
        rule: &'static str,
        /// The offending statement text.
        statement: String,
    },
    /// The SQL could not be parsed (deny-by-default: it never reaches the DB).
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    /// A raw SQL string reached a guard that cannot vet it, and was refused
    /// fail-closed rather than mis-vetted or waved through.
    ///
    /// # The message used to describe only one of the three ways this is reached
    ///
    /// It read "raw SQL is not accepted by the PostgreSQL parser guard for backend
    /// {dialect}", which was TRUE of one producer and FALSE of the other two. All
    /// three shipping guards raise it, each carrying its OWN id:
    ///
    /// * the parser-backed guard, when the config's target is not the dialect its
    ///   parser reads - it cannot vet another grammar, so it declines;
    /// * the other two, from `check_raw_island_sql` / `check_raw_island_body`,
    ///   because they have NO raw door at all. Answering `Ok` there would grant an
    ///   unchecked raw path no author of that dialect can even open.
    ///
    /// So on two of the three the refusal came from the target's own guard, and the
    /// message told the operator a different backend's parser had turned it away. It
    /// names the refusing target and nothing else now.
    ///
    /// The id is PROVENANCE only: no behaviour dispatches on it, and a fourth backend
    /// gets this refusal by declining in its own name.
    #[error(
        "raw SQL is not accepted by {dialect}'s registered guard: it cannot vet this \
         text, so the statement is refused rather than mis-vetted"
    )]
    RawSqlRejected {
        /// The backend whose guard refused the text, from its own identity.
        dialect: DialectId,
    },
}

/// The **dialect-neutral** result of a passing [`MigrationGuard::check`].
///
/// This is the line-1 output the core engine actually consumes: the engine's
/// `plan()`/`apply` only read `destructive` (to drive the destructive/approval
/// gate) and `advisories` (to surface operational footguns) - see
/// `MigrationEngine::plan`. Deliberately **does not** carry the
/// PG-specific `classes: Vec<StatementClass>` (the `libpg_query` `DdlKind`
/// vocabulary): that stays *inside* the PG guard (`SqlGuard`/`GuardReport`),
/// because a non-PG engine (`SQLite` descriptor diff, a future non-PG parser) has no
/// `DdlKind` to populate. Keeping the neutral seam free of PG vocabulary is
/// what lets a new engine bring its own line-1 without inheriting `libpg_query`.
///
/// The PG-only consumers of `classes` (`flags_for`, the author/submit/loader
/// flag derivation, the `guard_security` matrix) keep calling `SqlGuard::check`
/// directly and keep the rich `GuardReport`; only the engine seam is neutral.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardOutcome {
    /// True if *any* statement is destructive (data loss). The engine's gate
    /// decides on approval; the guard only flags.
    pub destructive: bool,
    /// Operational [`Advisory`]s (lock-heavy ops,
    /// backward-incompatible shapes, missing FK indexes, ...). Advisory-only -
    /// never deny or gate. Empty for engines that emit none (e.g. `SQLite`'s
    /// descriptor path).
    pub advisories: Vec<Advisory>,
}

/// The **per-vendor line-1 defense** - the third thing a backend crate supplies,
/// alongside its [`crate::renderer::DmlRenderer`] and [`crate::schema::SchemaRenderer`].
///
/// The core engine never selects a guard by dialect (`if dialect == Sqlite`). It
/// reads [`crate::registry::BackendVendor::guard`] and runs whatever line-1 that
/// vendor brought.
///
/// - **PostgreSQL** - the `libpg_query` parse + deny-list + classify + analyze
///   (`SqlGuard`), mapped onto the neutral [`GuardOutcome`].
/// - **`SQLite`** and **`MySQL`** - the descriptor-diff path is trusted by
///   construction (validated at the author boundary, line-2 enforced by the backend's
///   runtime authorizer at apply), so `check` returns the **empty/clean** outcome.
///   Each vendor writes its OWN trusting impl rather than sharing one type.
/// - A future non-PostgreSQL engine brings its own parser/allowlist impl. It has to
///   bring SOMETHING: this method has no default body, so a vendor cannot inherit a
///   trusting guard by omission.
///
/// # `check` is deliberately not defaulted
///
/// Giving [`MigrationGuard::check`] a default `Ok(GuardOutcome::default())` body
/// would make "ship no guard" compile into "trust everything" - the failure mode this
/// whole seam exists to prevent. Trust must be TYPED OUT to be granted.
///
/// # What this trait does NOT cover
///
/// The empty outcome is not the whole story for a descriptor-only vendor.
/// `data_security.destructive_ops = forbid` is enforced for SQLite and MySQL by
/// [`check_ir_data_security_policy`], over the structured IR
/// rather than over SQL text, precisely BECAUSE their guard here is empty and is
/// constructed without the policy. Reading only this trait would leave you believing
/// those two dialects have no data-security posture at all. They do; it is enforced
/// one layer up, at the IR.
///
/// `GuardOutcome` / [`GuardError`] are shared + neutral; each vendor's parser is its
/// own concern.
pub trait MigrationGuard {
    /// Run line-1 over a migration's `up` SQL. `Ok(GuardOutcome)` when every
    /// statement is safe (destructive ops flagged, not denied); `Err` on the
    /// first hard-denied / cross-tenant / unparseable / raw-rejected construct.
    ///
    /// # Errors
    /// Vendor-specific: PostgreSQL surfaces [`GuardError::Denied`] /
    /// [`GuardError::CrossSchema`] / [`GuardError::Parse`]; the descriptor-only
    /// vendors do not deny (they trust), so their `check` is infallible in practice.
    fn check(&self, up: &str) -> Result<GuardOutcome, GuardError>;

    /// Line-1 BACKSTOP over one rendered raw-SQL island: the narrower "even text the
    /// ordinary statement-kind gate would wave through may not do THIS" set. A vendor
    /// that answers `Ok` unconditionally is granting an unchecked raw door, and must
    /// say so in its own doc.
    ///
    /// # This method currently has no caller on the lowering path
    ///
    /// It was reached from `render::lower`'s guarded lowering ONLY for a config whose
    /// root/host-set mode had skipped the deny-list belt - the removed Trusted
    /// posture. Every config the engine can build now runs the full belt through
    /// [`MigrationGuard::check`], so nothing calls this. It is kept as the declared
    /// vendor answer to "what survives a belt-skip", because reinstating an
    /// unconfined posture without it would hand that posture an unchecked raw door,
    /// and each vendor's answer is already written and tested here.
    ///
    /// # Errors
    /// Vendor-specific, in the same vocabulary as [`MigrationGuard::check`].
    fn check_raw_island_sql(&self, sql: &str) -> Result<(), GuardError>;

    /// Line-1 BACKSTOP over one raw FUNCTION BODY, the peer of
    /// [`MigrationGuard::check_raw_island_sql`] and uncalled on the lowering path for
    /// the same reason.
    ///
    /// A body is not a statement list: a procedural body is only best-effort
    /// parseable, so a vendor's answer here is generally a token/literal scan rather
    /// than a parse. `raw` is the rendered statement the body came from, carried so a
    /// denial can name the text the operator wrote.
    ///
    /// # Errors
    /// Vendor-specific, in the same vocabulary as [`MigrationGuard::check`].
    fn check_raw_island_body(&self, body: &str, raw: &str) -> Result<(), GuardError>;

    /// Does THIS guard already refuse a destructive operation under
    /// `data_security.destructive_ops = forbid`, on its own?
    ///
    /// # Answering `true` TURNS OFF a belt, so answer it deliberately
    ///
    /// [`check_ir_data_security_policy`] runs a neutral posture walk over the
    /// structured ops, and skips it for a guard that answers `true` here. For a guard
    /// that reads the knob itself, running the neutral walk as well would produce a
    /// second, EARLIER denial and change a refusal existing assertions pin, for no
    /// behavioural gain. For a guard that does NOT read it, the neutral walk is the
    /// only enforcement that knob has: a guard constructed without the policy cannot
    /// see it at all.
    ///
    /// So `false` is the SAFE answer and it is the one a guard should give unless it
    /// can point at where it refuses. Required with no default body, like every other
    /// method here, so a new backend cannot acquire `true` by omission.
    ///
    /// # The guard answers, not a vendor comparison
    ///
    /// Core must not decide a SECURITY posture by naming one vendor: a gate like
    /// `if cfg.dialect() != &POSTGRES` would have a fourth backend inherit the
    /// answer from not being that vendor rather than from anything about its guard.
    /// This module's own header states the general move: when neutral
    /// code seems to need a vendor's tool, find the single question it is using the
    /// tool to answer and make THAT the vendor's method.
    fn refuses_destructive_ops_itself(&self) -> bool;

    /// Can this vendor NOT enumerate the net table state of a raw SQL island - i.e.
    /// does the island escape [`check_ir_data_security_policy`]'s `safety.require_rls`
    /// net-state walk?
    ///
    /// This is the hook that lets the neutral walk stay neutral. The walk decides
    /// `require_rls` over structured ops, which it can do for every dialect; a raw
    /// island is the one op whose net table state is not derivable from the IR. Only
    /// the vendor that owns the raw door can answer, and a vendor with NO raw door
    /// answers `false` without anyone parsing anything.
    ///
    /// `true` means "refuse this island" - either because it reaches an obligated
    /// relation or because the vendor cannot attribute it at all. Answering `false`
    /// unconditionally while the vendor DOES accept raw text would let raw SQL create
    /// an RLS-less table behind the obligation's back, so a vendor that answers
    /// `false` must state which of the two reasons it is.
    fn raw_island_escapes_rls_net_state(&self, sql: &str) -> bool;

    /// Derive a migration's [`MigrationFlags`] from its `up` SQL.
    ///
    /// The raw-SQL author hands the engine pre-generated SQL and no flags, so the
    /// destructive / non-transactional / approval facets have to be read back OUT of
    /// the text. Only a vendor that can classify its own statements can do that, and
    /// what "destructive" or "non-transactional" means is that vendor's grammar.
    ///
    /// A *denial* is not this method's to raise - the engine's `plan()` re-runs the
    /// guard and reports denials so a caller sees every problem at once. A vendor
    /// that cannot classify a denied-but-parseable statement should return
    /// conservative flags (`requires_approval: true`) rather than an error.
    ///
    /// # Errors
    /// Only when `up` cannot be classified at all (a parse failure), which is an
    /// authoring-time error the author surfaces immediately.
    fn flags_for_sql(&self, up: &str) -> Result<MigrationFlags, GuardError>;
}

/// A data-security policy failure attributed to an IR op index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrDataSecurityError {
    /// The op that violated the data-security policy.
    pub op_index: usize,
    /// The guard policy error.
    pub source: GuardError,
}

/// The per-table RLS net state the [`check_ir_data_security_policy`] walk accumulates.
#[derive(Debug, Clone)]
struct RlsTableState {
    exists_after: bool,
    rls_enabled: bool,
    last_op_index: usize,
    table: String,
}

/// Does a `safety.require_rls` obligation cover the table this policy key names?
///
/// [`table_key_for_policy`] yields an EMPTY schema for an unqualified table under a
/// config with an empty target, and no `ObjectName` can be built from `.users`.
/// [`GuardConfig::requires_rls_at_table`] is where that fall-back-closed rule lives,
/// so the declarative path resolves the obligation the same way this walk does.
fn require_rls_covers(cfg: &GuardConfig, key: &(String, String)) -> bool {
    let (schema, table) = key;
    cfg.requires_rls_at_table(schema, table)
}

fn table_key_for_policy(
    cfg: &GuardConfig,
    schema: &Option<String>,
    table: &str,
) -> (String, String) {
    let effective_schema = schema
        .clone()
        .unwrap_or_else(|| cfg.project_schema().to_string());
    (effective_schema, table.to_string())
}

/// Enforce data-security knobs that require the structured IR op set.
///
/// `require_rls` is a cross-op obligation over the migration's final table RLS
/// state, not a textual co-occurrence rule. Any table this migration creates and
/// leaves present must end RLS-enabled; any attempt to turn RLS/force off is refused
/// outright. Raw SQL islands are rejected because the guard cannot enumerate their
/// net table state fail-closed.
///
/// `safety.require_rls` is registered `ObjectModel::PerTable`, so every one of those
/// decisions resolves at the CONCRETE table it is about, the same way
/// `zeroship_migrate_ir::policy_approval` resolves its sibling `safety.require_approval`.
/// The net-state walk therefore runs unconditionally: whether an obligation covers a
/// table is a property of that table, not of the migration.
///
/// # Why this is neutral, and where the one vendor question went
///
/// Every decision below is read off a structured [`Op`] - a create, a drop, a rename,
/// a `setRls` - which every dialect emits and none of them needs a parser to
/// understand. The single exception is the raw island: `Op::Raw` carries text, and
/// text's net table state is not derivable from the IR. That one question is asked of
/// `guard` through [`MigrationGuard::raw_island_escapes_rls_net_state`], so the vendor
/// that owns the raw door answers it and a descriptor-only vendor answers "I have no
/// raw door" without anyone parsing anything.
///
/// This is what lets the walk live below every vendor: it is the ONLY
/// `destructive_ops = forbid` enforcement SQLite and MySQL have (their line-1 guard is
/// the empty trusting one and is constructed without the policy), and it now reaches
/// no parser to provide it.
///
/// # Errors
/// [`IrDataSecurityError`] naming the op index that violated the policy.
pub fn check_ir_data_security_policy(
    cfg: &GuardConfig,
    ir: &MigrationIr,
    guard: &dyn MigrationGuard,
) -> Result<(), IrDataSecurityError> {
    fn push_policy_ops<'a>(
        cfg: &GuardConfig,
        op_index: usize,
        op: &'a Op,
        out: &mut Vec<(usize, &'a Op)>,
    ) {
        if let Op::Dialectal { legs } = op {
            if let Some(leg) = legs.get(cfg.dialect()) {
                for inner in leg {
                    push_policy_ops(cfg, op_index, inner, out);
                }
            }
        } else {
            out.push((op_index, op));
        }
    }

    let mut policy_ops = Vec::new();
    for (op_index, op) in ir.ops.iter().enumerate() {
        push_policy_ops(cfg, op_index, op, &mut policy_ops);
    }

    // The destructive posture is a property of the OPERATION, not of any SQL text,
    // so it is enforced here, where every dialect can see it.
    //
    // A guard that reads the same knob in its own text guard and refuses there is
    // skipped, because a second, earlier denial would change a refusal that existing
    // assertions pin, for no behavioural gain. A guard constructed WITHOUT the policy
    // cannot read the knob at all, and for those this walk is the only enforcement
    // the posture has.
    //
    // WHICH of the two a guard is is the GUARD's answer
    // ([`MigrationGuard::refuses_destructive_ops_itself`]), not a dialect comparison:
    // core must not decide a security posture by naming a vendor.
    //
    // `policy_ops` is used rather than `ir.ops` so a `Dialectal` op is judged by
    // the leg THIS dialect will actually run.
    // NOT `Op::is_destructive` on its own. That is the APPROVAL notion, and it is
    // wider on purpose: `safety.require_approval = on_destructive` reasonably wants
    // a human to look at row-affecting DML. The POSTURE must match what PostgreSQL
    // actually denies, which is observed server behavior rather than the
    // classifier's answer - `destructive_update_operation` returns
    // `Some("UPDATE")` unconditionally, yet PostgreSQL applies a bounded `update`
    // under the default `forbid`, because DML lowers to a bound `PlanStep::Dml`
    // that the SQL-text guard never inspects.
    //
    // Enforcing the wider notion here would make MySQL and SQLite STRICTER than
    // PostgreSQL - an `update` that PostgreSQL applies would be refused - which
    // is a regression, not parity. Row DML is therefore excluded, leaving the
    // object-drop and lossy-DDL family that PostgreSQL's guard does deny.
    //
    // `Raw` is excluded because it cannot reach a guard that answers `false` above:
    // a backend with no raw door refuses the island in its own name, and the
    // parser-backed guard refuses text it cannot vet.
    let posture_denies = |op: &Op| {
        op.is_destructive()
            && !matches!(
                op,
                Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } | Op::Raw { .. }
            )
    };
    if !guard.refuses_destructive_ops_itself()
        && matches!(cfg.destructive_ops(), DestructiveOps::Forbid)
    {
        if let Some(&(op_index, _)) = policy_ops.iter().find(|(_, op)| posture_denies(op)) {
            return Err(IrDataSecurityError {
                op_index,
                source: GuardError::DataSecurityPolicy {
                    rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                    statement: "destructive operation denied while \
                                data_security.destructive_ops=forbid"
                        .to_string(),
                },
            });
        }
    }

    let mut tables: BTreeMap<(String, String), RlsTableState> = BTreeMap::new();
    for (op_index, op) in policy_ops {
        match op {
            Op::CreateTable { name, schema, .. } | Op::CreatePartition { name, schema, .. } => {
                let key = table_key_for_policy(cfg, schema, name);
                tables.insert(
                    key,
                    RlsTableState {
                        exists_after: true,
                        rls_enabled: false,
                        last_op_index: op_index,
                        table: name.clone(),
                    },
                );
            }
            Op::SetRls {
                table,
                schema,
                enabled,
                forced,
            } => {
                // Turning RLS or its force flag off is refused at the table THIS op
                // names, not at the migration: an obligation over `app.users` says
                // nothing about `app.audit`.
                let obligated = require_rls_covers(cfg, &table_key_for_policy(cfg, schema, table));
                if obligated && enabled == &Some(false) {
                    return Err(IrDataSecurityError {
                        op_index,
                        source: GuardError::DataSecurityPolicy {
                            rule: data_security_rule::REQUIRE_RLS,
                            statement: format!(
                                "setRls {table:?} enabled:false is forbidden while data_security.require_rls=true"
                            ),
                        },
                    });
                }
                if obligated && forced == &Some(false) {
                    return Err(IrDataSecurityError {
                        op_index,
                        source: GuardError::DataSecurityPolicy {
                            rule: data_security_rule::REQUIRE_RLS,
                            statement: format!(
                                "setRls {table:?} forced:false is forbidden while data_security.require_rls=true"
                            ),
                        },
                    });
                }
                if enabled == &Some(true) {
                    let key = table_key_for_policy(cfg, schema, table);
                    tables
                        .entry(key)
                        .and_modify(|state| {
                            state.exists_after = true;
                            state.rls_enabled = true;
                            state.last_op_index = op_index;
                            state.table = table.clone();
                        })
                        .or_insert_with(|| RlsTableState {
                            exists_after: true,
                            rls_enabled: true,
                            last_op_index: op_index,
                            table: table.clone(),
                        });
                }
            }
            Op::AttachPartition { .. } | Op::DetachPartition { .. } => {}
            Op::DropTable { table, schema, .. }
            | Op::DropPartition {
                name: table,
                schema,
                ..
            } => {
                let key = table_key_for_policy(cfg, schema, table);
                tables
                    .entry(key)
                    .and_modify(|state| {
                        state.exists_after = false;
                        state.rls_enabled = false;
                        state.last_op_index = op_index;
                        state.table = table.clone();
                    })
                    .or_insert_with(|| RlsTableState {
                        exists_after: false,
                        rls_enabled: false,
                        last_op_index: op_index,
                        table: table.clone(),
                    });
            }
            Op::RenameTable {
                table, to, schema, ..
            } => {
                let from = table_key_for_policy(cfg, schema, table);
                let to_key = table_key_for_policy(cfg, schema, to);
                if let Some(mut state) = tables.remove(&from) {
                    state.last_op_index = op_index;
                    state.table = to.clone();
                    tables.insert(to_key, state);
                }
            }
            Op::Raw { sql, .. }
                if cfg.require_rls_authored_anywhere()
                    && guard.raw_island_escapes_rls_net_state(sql) =>
            {
                return Err(IrDataSecurityError {
                    op_index,
                    source: GuardError::DataSecurityPolicy {
                        rule: data_security_rule::REQUIRE_RLS,
                        statement: "raw is forbidden while data_security.require_rls=true because raw SQL can create tables outside the structured RLS net-state check".to_string(),
                    },
                });
            }
            _ => {}
        }
    }

    for (key, state) in &tables {
        if state.exists_after && !state.rls_enabled && require_rls_covers(cfg, key) {
            return Err(IrDataSecurityError {
                op_index: state.last_op_index,
                source: GuardError::DataSecurityPolicy {
                    rule: data_security_rule::REQUIRE_RLS,
                    statement: format!(
                        "table {:?} must end this migration with row level security enabled",
                        state.table
                    ),
                },
            });
        }
    }

    Ok(())
}

/// A stable concrete object for a Global-model capability query. Every Global grant
/// (whole-universe scope, or absent) resolves the same at every object, so any
/// witness decides it; `zsg` is an arbitrary fixed schema that never collides with a
/// real target (the value is irrelevant for a whole-universe or default grant).
///
/// The soundness condition, that the knob really is `ObjectModel::Global`, is checked
/// against the registry by [`GuardConfig::global_witness_for`], which is how every
/// Global read reaches this. [`GuardConfig::grants_bool_everywhere`] also reaches it
/// directly, having established the same "resolves identically everywhere" condition
/// a different way: from the rule's whole-universe grant region rather than from the
/// knob's object model.
fn global_witness() -> ObjectName {
    ObjectName::schema(b"zsg".to_vec())
}

/// What one covering `inject` rule requires a create at its scope to carry, in the
/// PostgreSQL identifier bytes its policy-authored names denote: every contributed
/// column name, plus the exact primary key when the rule pins one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedCreateShape {
    columns: Vec<Vec<u8>>,
    primary_key: Option<Vec<Vec<u8>>>,
}

impl InjectedCreateShape {
    /// Does `declared` prove this inject rule was honoured? A column is satisfied
    /// when the create declares a column of that exact name; a pinned primary key is
    /// satisfied only by the SAME columns in the SAME order, because
    /// `PRIMARY KEY (id, tenant_id)` against a pinned `(id)` is a different key, not
    /// a superset of one.
    ///
    /// A rule that contributes no column and pins no key demands nothing a create's
    /// text could exhibit (`columns` is optional, so an inject carrying only
    /// `indexes` is a legal charter). It is never satisfied: an obligation the
    /// statement cannot carry proof of leaves the create unprovable, so it is denied
    /// rather than admitted by a vacuously true check.
    pub fn is_satisfied_by(&self, declared: &DeclaredCreateShape) -> bool {
        if self.columns.is_empty() && self.primary_key.is_none() {
            return false;
        }
        self.columns
            .iter()
            .all(|column| declared.columns.contains(column))
            && self
                .primary_key
                .as_ref()
                .is_none_or(|pinned| declared.primary_key.as_ref() == Some(pinned))
    }
}

/// What a `CREATE TABLE` statement's own text DECLARES, in PostgreSQL identifier
/// bytes: the column names it lists and the ordered primary-key column list it
/// declares (table-level `PRIMARY KEY (...)` or a column-level marker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredCreateShape {
    columns: BTreeSet<Vec<u8>>,
    primary_key: Option<Vec<Vec<u8>>>,
}

impl DeclaredCreateShape {
    /// Build the declared shape a vendor's parser read out of a `CREATE TABLE`.
    ///
    /// The SHAPE is neutral - a set of column-name bytes and an ordered key - but
    /// reading it out of a statement is not, so the READER stays in the vendor crate
    /// that owns the parser and hands the result here.
    #[must_use]
    pub const fn new(columns: BTreeSet<Vec<u8>>, primary_key: Option<Vec<Vec<u8>>>) -> Self {
        Self {
            columns,
            primary_key,
        }
    }

    /// Does this create satisfy every covering inject rule?
    #[must_use]
    pub fn conforms_to(&self, injects: &[InjectedCreateShape]) -> bool {
        injects.iter().all(|inject| inject.is_satisfied_by(self))
    }
}

/// Fold one POLICY-authored identifier to the PostgreSQL identifier bytes it
/// denotes (II.2.7: an unquoted name lowercases, a quoted one stays verbatim), so a
/// charter's `created_at` and `"Created_At"` name the columns they would name in
/// SQL. Only the policy side is folded; a name read out of a parsed statement is
/// already the resolved identifier.
///
/// A name that normalizes to two segments (it contains an unquoted dot) folds
/// verbatim-lowercased rather than to its leading segment, so a charter column
/// `a.b` demands a column literally named `a.b`. That diverges from the policy
/// crate's `names_match`, which folds `a.b` to `a` and would demand a column named
/// `a` instead. The divergence is a difference in which column the charter names,
/// not a safety property in either direction; the two folds should be unified once
/// the policy crate exposes its single-identifier fold.
fn fold_identifier(name: &str) -> Vec<u8> {
    match normalize_object_name(name) {
        Some(object) if object.table.is_none() => object.schema,
        _ => name.to_ascii_lowercase().into_bytes(),
    }
}
