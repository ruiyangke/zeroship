//! The LINE-1 CONTRACT — what a backend crate's security guard must satisfy.
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
//! already depends on every vendor — a crate cycle Cargo refuses. The arrow only
//! works one way: the contract sits BELOW every vendor, the vendors implement it,
//! and the engine composes them. That is the same reason
//! [`crate::registry::BackendVendor`] lives here.
//!
//! # What moved here, and what deliberately did NOT
//!
//! What moved is the SEAM: [`GuardConfig`], [`GuardMode`], [`GuardError`],
//! [`GuardOutcome`], [`ParseError`] and [`IrDataSecurityError`], plus
//! [`crate::advisory`]. Every vendor's guard is configured by the same unforgeable
//! [`EffectivePolicy`] and reports in the same vocabulary, so the seam is genuinely
//! neutral and belongs below every vendor.
//!
//! What did NOT move is `zero-migrate-guard` itself, and that is a MEASURED refusal
//! rather than an unfinished job. The obvious next step — dissolve that crate into
//! the vendors so each ships its own line-1 — does not decompose:
//!
//! - `check_ir_data_security_policy` is the ONLY `destructive_ops = forbid`
//!   enforcement SQLite and MySQL have. Its gate is literally
//!   `if cfg.dialect() != &POSTGRES`, and its own comment says
//!   why: the descriptor guard those two dialects run is constructed without the
//!   policy and cannot read the knob at all.
//! - That same function reaches `pg_query::parse` for `Op::PgRaw` islands, because a
//!   raw island's net table state is not enumerable without a parser.
//!
//! So the function that enforces SQLite's and MySQL's posture needs PostgreSQL's
//! parser. Putting it in `zero-migrate-postgres` would file two dialects' only
//! data-security enforcement under a third dialect's crate; putting it HERE would
//! push `libpg_query` beneath `zero-migrate-sqlite` and `zero-migrate-mysql`, which
//! today build without it. Both are worse than leaving it where it is. The smallest
//! change that WOULD free it is a per-vendor "can you enumerate this raw island's net
//! table state?" hook, so a descriptor-only vendor can answer "I have no raw door"
//! without anyone parsing anything — a behaviour change, not a move.
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
//! This is what replaces the old `guard_for(cfg) -> Box<dyn MigrationGuard>` dispatch,
//! whose closed three-way match handed BOTH descriptor-only dialects one shared
//! `SqliteDescriptorGuard`. That match was exhaustive, so a fourth dialect broke it —
//! but a `_ =>` arm added in haste would have granted every future backend the
//! trusting path silently. The required field removes the arm the mistake could be
//! made in. `BackendVendor` therefore derives no `Default`, has no `Default` impl, is
//! not `#[non_exhaustive]`, and its `guard` field is not an `Option` — each of those
//! would reintroduce exactly the silent grant of trust this removes.

use std::collections::BTreeSet;

use zero_migrate_ir::dialect::{DialectId, POSTGRES};
use zero_migrate_ir::policy::DestructiveOps;
use zero_migrate_ir::policy::SchemaScope;
use zero_migrate_ir::policy_registry;
use zero_migrate_policy::{
    normalize_pg_identifier, EffectivePolicy, GrantRegion, KnobKey, KnobValue, ObjectModel,
    ObjectName, ShapeElement,
};

use crate::advisory::Advisory;

/// Error parsing SQL for classification.
///
/// Neutral because [`GuardError::Parse`] carries it and [`GuardError`] is the
/// vocabulary every vendor's guard reports in. The PARSER that raises it is not
/// neutral — today only `zero-migrate-postgres` has one.
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

/// The engine-construction POSTURE that decides whether the static parse-time guard
/// belt runs at all. This is NOT a composable policy knob: "run without the deny-list
/// guard" is the single most dangerous switch, so it is a root/host-set posture on the
/// guard config — it can neither be granted, inherited, nor drafted by a creator.
///
/// - `Enforced` (the default) — the full belt runs: the deny-list, cross-schema
///   confinement, and body walks. Confined and Platform both run `Enforced`.
/// - `Off` - the public dbmate-like Trusted posture: the operator owns the
///   DB, so there is NO untrusted boundary and the whole belt is skipped (arbitrary
///   SQL applies as the connecting role). Raw islands embedded in structured IR still
///   run their deny-list backstop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// Run the full static parse-time guard belt (Confined / Platform).
    Enforced,
    /// Skip the belt entirely — the Trusted dbmate-like posture (host owns the DB).
    Off,
}

/// Per-guard configuration.
///
/// All fields are private. A caller must supply an explicitly composed
/// [`EffectivePolicy`] through [`GuardConfig::from_policy`] or
/// [`GuardConfig::from_policy_with_mode`]. The guard never selects or fabricates a
/// policy from a named posture.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// PRIVATE. The target SQL dialect this guard config is for.
    ///
    /// - `postgres` — the `libpg_query` line-1 guard runs
    ///   (`SqlGuard::check` parses + deny-walks the SQL).
    /// - every other id — a vendor-owned guard path. An untrusted raw SQL string
    ///   presented to PostgreSQL's `SqlGuard::check` is refused. Any explicit
    ///   non-enforced mode is reset to [`GuardMode::Enforced`] by
    ///   [`GuardConfig::for_dialect`]. This comparison is deliberately open and
    ///   fail-closed: a future backend cannot inherit PostgreSQL's belt-off mode.
    dialect: DialectId,
    /// PRIVATE. The unforgeable [`EffectivePolicy`] — the SINGLE source the guard's
    /// every composable decision queries. The capability gate asks `grants(key,
    /// object)` for the statement's builtin knob key; cross-schema confinement asks
    /// `grants(schema.cross_schema, schema)`; the data-security obligations read
    /// `obligations`/`grants` on the safety.* knobs. There is no separate posture/scope
    /// state for the composable knobs — the policy IS the posture.
    effective: EffectivePolicy,
    /// PRIVATE. The root/host-set [`GuardMode`] — whether the static parse-time belt
    /// runs. This is NOT a composable knob (it quarantines the "skip the guard" switch
    /// out of the policy registry); the belt-skip reads it, and the raw-island
    /// role-needle relaxation keys off it + the `access.role` grant internally.
    guard_mode: GuardMode,
}

impl GuardConfig {
    /// Construct a guard directly from one composed [`EffectivePolicy`] + dialect, at
    /// the default [`GuardMode::Enforced`] posture (the full belt runs). The effective
    /// policy is the SINGLE source for injection and every composable guard decision.
    #[must_use]
    pub fn from_policy(effective: EffectivePolicy, dialect: DialectId) -> Self {
        Self::from_policy_with_mode(effective, dialect, GuardMode::Enforced)
    }

    /// Construct a guard from a composed [`EffectivePolicy`] + dialect + an explicit
    /// root/host-set [`GuardMode`]. `GuardMode::Off` is the Trusted dbmate-like posture
    /// (belt skipped). The mode is NOT derivable from the policy — it is a posture the
    /// host sets, never a composable grant.
    #[must_use]
    pub fn from_policy_with_mode(
        effective: EffectivePolicy,
        dialect: DialectId,
        guard_mode: GuardMode,
    ) -> Self {
        Self {
            dialect,
            effective,
            guard_mode,
        }
    }

    /// Replace the composed policy while preserving this config's dialect and
    /// host-selected guard mode.
    #[must_use]
    pub fn with_effective_policy(mut self, effective: EffectivePolicy) -> Self {
        self.effective = effective;
        self
    }

    /// Borrow the composed [`EffectivePolicy`] this config decides against.
    ///
    /// This exists so the `effective` FIELD can stay private now that a vendor's
    /// guard lives one crate above the config it reads. It is a read-only borrow of a
    /// policy the caller already holds — it grants nothing that
    /// [`GuardConfig::with_effective_policy`] does not already allow, and it is what
    /// keeps the `compile_fail` struct-literal boundary below intact: an external
    /// crate still cannot NAME any of the three fields.
    #[must_use]
    pub const fn effective(&self) -> &EffectivePolicy {
        &self.effective
    }

    /// Select a target dialect without changing the caller-composed policy.
    /// PostgreSQL preserves the selected guard mode. Every other id forces
    /// [`GuardMode::Enforced`]. This must remain an explicit comparison against
    /// [`POSTGRES`], never a self-declared capability: a future backend must not be
    /// able to declare its way out of the fail-safe posture.
    #[must_use]
    pub fn for_dialect(mut self, dialect: DialectId) -> Self {
        if dialect != POSTGRES {
            self.guard_mode = GuardMode::Enforced;
        }
        self.dialect = dialect;
        self
    }

    /// The target SQL dialect this guard config vets.
    #[must_use]
    pub const fn dialect(&self) -> &DialectId {
        &self.dialect
    }

    /// Whether this config skips the confined deny-list belt entirely (the Trusted
    /// dbmate-like posture) — the root/host-set [`GuardMode::Off`]. `pub`: the engine's
    /// profile behaviour-lock tests assert it across the crate boundary.
    #[must_use]
    pub fn skips_denylist_belt(&self) -> bool {
        matches!(self.guard_mode, GuardMode::Off)
    }

    /// The root/host-set [`GuardMode`] posture this config carries.
    #[must_use]
    pub fn guard_mode(&self) -> GuardMode {
        self.guard_mode
    }

    /// The schema-confinement scope this guard config enforces, for the
    /// validate-time cross-schema gate. Derived from the effective policy's
    /// `schema.cross_schema` grant:
    /// - a `⊤` (whole-universe) grant ⇒ `Unconfined` (the Trusted operator posture);
    /// - a finite set of owned schemas ⇒ `Single(s)` for one, `Allowlist([…])` for
    ///   several (Confined / Platform);
    /// - no owned schema (empty) ⇒ `Single("")` (the degenerate default).
    ///
    /// This is the SINGLE source of truth that maps the policy to the validator's
    /// confinement scope, so the parse-guard cross-schema denial (line 1) and the
    /// friendlier validate-time refusal agree on the permitted set.
    #[must_use]
    pub fn schema_scope(&self) -> Option<SchemaScope> {
        let key = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok()?;
        if matches!(self.effective.grant_region(&key), GrantRegion::Top) {
            return Some(SchemaScope::Unconfined);
        }
        let owned = owned_schemas_from_effective(&self.effective);
        // The operator (Platform) posture is a schema ALLOWLIST even for a single
        // owned schema — it grants the privileged vendor set (`access.role`); a Confined
        // posture (no privileged caps) with one owned schema is a `Single` pin.
        let is_operator_posture = self.grants_global_bool(policy_registry::KEY_ACCESS_ROLE);
        Some(match owned.as_slice() {
            [one] if !is_operator_posture => SchemaScope::Single(one.clone()),
            [] if !is_operator_posture => SchemaScope::Single(String::new()),
            _ => SchemaScope::Allowlist(owned),
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
    // raw `VendorCapabilities` bit / `require_rls` / `destructive_ops` field. All
    // scope resolution lives inside the `EffectivePolicy`; the guard passes a
    // concrete object and reads back a value.

    /// Does the effective policy GRANT the whole-DB (Global) capability `key` at
    /// `object`? A Global Bool grant resolves the same at every object; we pass a
    /// stable global witness. Reproduces `caps.grants(cap)`: absent grant ⇒ the
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
    /// relation under a charter with no unique owned schema, a `CREATE SCHEMA
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

    /// Whether the effective policy holds the EXTENSION capability at all — i.e. the
    /// `code.extension` StrSet allowlist is non-empty. The allowlist IS the capability:
    /// empty = deny all (so no CREATE/DROP EXTENSION). `FORBIDDEN_EXTENSIONS` still
    /// overrides which specific names may be created.
    pub fn grants_extension_capability(&self) -> bool {
        !self.granted_extension_allowlist().is_empty()
    }

    /// The permitted `CREATE EXTENSION` names granted by the effective policy — the
    /// `code.extension` StrSet grant value at the global witness. Empty when no
    /// grant covers it (deny-by-default). `FORBIDDEN_EXTENSIONS` still overrides
    /// this in the guard regardless.
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
    /// The object is built THROUGH [`normalize_pg_identifier`], the same way
    /// `zero_migrate_ir::policy_approval` builds its own: the composer's scope matcher
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
        match normalize_pg_identifier(&format!("{schema}.{table}")) {
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

    /// The effective destructive-op posture — the `safety.destructive_ops` OrderedEnum
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

    /// The project schema an UNQUALIFIED relation resolves to under this config's
    /// cross-schema pin — the sole schema owned by the effective policy's
    /// `schema.cross_schema` grant. `None` when there is no unique owned schema (empty
    /// confined default, multi-schema platform, or a `⊤` grant) — an unqualified name
    /// is then not uniquely attributable by the guard.
    pub fn pinned_schema(&self) -> Option<String> {
        match owned_schemas_from_effective(&self.effective).as_slice() {
            [one] if !one.is_empty() => Some(one.clone()),
            _ => None,
        }
    }

    /// True iff the effective policy grants Bool `key` (default-deny) at the concrete
    /// normalized `object`. Fails closed (`false`) on an unknown key or non-Bool value.
    pub fn grants_namespace_bool(&self, key: &str, object: &ObjectName) -> bool {
        self.grants_bool_at(key, object)
    }

    /// The [`GrantRegion`] of `sql.raw` — the ⊤/Scoped/Ungranted posture the
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

    /// Cross-schema confinement, decided directly on the PDP: is a reference to
    /// `schema` admitted? True iff the effective policy grants `schema.cross_schema`
    /// at the (PG-normalized) schema object — the project schema(s) a confined/
    /// platform posture owns are granted; every other schema is a `CrossSchema`
    /// violation (default-deny). An un-normalizable schema name (empty / malformed)
    /// is NOT admitted (fail-closed). This replaces the derived-`SchemaScope`
    /// `permits(schema)` read.
    pub fn grants_cross_schema(&self, schema: &str) -> bool {
        let Some(k) = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok() else {
            return false;
        };
        let Some(object) = normalize_pg_identifier(schema) else {
            return false;
        };
        matches!(
            self.effective.grants(&k, &object),
            Some(KnobValue::Bool(true))
        )
    }
}

/// T8 — the EXTERNAL trust boundary, pinned as `compile_fail` doctests. A doctest
/// is compiled as a SEPARATE crate that `use`s `zero_migrate_backend`, so it
/// exercises exactly the boundary an external consumer of this crate sits behind.
///
/// KEEP THE FIELD LISTS BELOW EXACT. A `compile_fail` doctest passes when the code
/// fails to compile for ANY reason, so a literal naming a field that no longer
/// exists passes on the typo and stops testing privacy at all. Both of these did
/// exactly that until the lists were corrected: they would have passed unchanged
/// with every field made `pub`. When a field is added or renamed, update these and
/// re-check them the only way that means anything - make the fields `pub`, confirm
/// the doctests FAIL, then put the visibility back.
///
/// (1) An external crate cannot write a `GuardConfig { .. }` struct literal — the
/// fields (`dialect`, `effective`, `guard_mode`) are private, so a privileged
/// profile can never be forged by a literal (the `EffectivePolicy` is itself
/// unforgeable). This MUST fail to compile:
///
/// ```compile_fail
/// use zero_migrate_backend::guard::GuardConfig;
/// let _ = GuardConfig {
///     dialect: zero_migrate_ir::dialect::DialectId::new("postgres"),
///     effective: unimplemented!(),
///     guard_mode: unimplemented!(),
/// };
/// ```
///
/// (2) The only unforgeable input to a privileged `GuardConfig` is a composed
/// `EffectivePolicy` — an external crate cannot construct one by a literal (its
/// fields are private and it has no public constructor other than `deny_all`). This
/// MUST fail to compile:
///
/// ```compile_fail
/// let _ = zero_migrate_policy::EffectivePolicy {
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
    /// A raw SQL string was presented to PostgreSQL's parser-backed guard for a
    /// different backend. `libpg_query` cannot vet another backend's grammar, so
    /// the text is refused fail-closed instead of being mis-vetted. The open id is
    /// provenance only: no behaviour dispatches on it, and a future backend gets
    /// this refusal automatically.
    #[error(
        "raw SQL is not accepted by the PostgreSQL parser guard for backend {dialect}: \
         that backend must use its own registered guard"
    )]
    RawSqlRejected {
        /// The backend whose SQL the PostgreSQL guard refused to mis-vet.
        dialect: DialectId,
    },
}

/// The **dialect-neutral** result of a passing [`MigrationGuard::check`].
///
/// This is the line-1 output the core engine actually consumes: the engine's
/// `plan()`/`apply` only read `destructive` (to drive the destructive/approval
/// gate) and `advisories` (to surface operational footguns) — see
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
    /// backward-incompatible shapes, missing FK indexes, …). Advisory-only —
    /// never deny or gate. Empty for engines that emit none (e.g. `SQLite`'s
    /// descriptor path).
    pub advisories: Vec<Advisory>,
}

/// The **per-vendor line-1 defense** — the third thing a backend crate supplies,
/// alongside its [`crate::renderer::DmlRenderer`] and [`crate::schema::SchemaRenderer`].
///
/// The core engine never selects a guard by dialect (`if dialect == Sqlite`). It
/// reads [`crate::registry::BackendVendor::guard`] and runs whatever line-1 that
/// vendor brought.
///
/// - **PostgreSQL** — the `libpg_query` parse + deny-list + classify + analyze
///   (`SqlGuard`), mapped onto the neutral [`GuardOutcome`].
/// - **`SQLite`** and **`MySQL`** — the descriptor-diff path is trusted by
///   construction (validated at the author boundary, line-2 enforced by the backend's
///   runtime authorizer at apply), so `check` returns the **empty/clean** outcome.
///   Each vendor writes its OWN trusting impl; they no longer share one type named
///   after only one of them.
/// - A future non-PostgreSQL engine brings its own parser/allowlist impl. It has to
///   bring SOMETHING: this method has no default body, so a vendor cannot inherit a
///   trusting guard by omission.
///
/// # `check` is deliberately not defaulted
///
/// Giving [`MigrationGuard::check`] a default `Ok(GuardOutcome::default())` body
/// would make "ship no guard" compile into "trust everything" — the failure mode this
/// whole seam exists to prevent. Trust must be TYPED OUT to be granted.
///
/// # What this trait does NOT cover
///
/// The empty outcome is not the whole story for a descriptor-only vendor.
/// `data_security.destructive_ops = forbid` is enforced for SQLite and MySQL by
/// `zero_migrate_guard::guard::check_ir_data_security_policy`, over the structured IR
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
}

/// A data-security policy failure attributed to an IR op index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrDataSecurityError {
    /// The op that violated the data-security policy.
    pub op_index: usize,
    /// The guard policy error.
    pub source: GuardError,
}

/// A stable concrete object for a Global-model capability query. Every Global grant
/// (`⊤`-scope or absent) resolves the same at every object, so any witness decides
/// it; `zsg` is an arbitrary fixed schema that never collides with a real target
/// (the value is irrelevant for a ⊤-scope / default grant).
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

/// The literal schemas an effective policy OWNS — the `schema.cross_schema` grant's
/// literal schema includes (the project schema(s) a confined/platform posture
/// carries). Empty for a `⊤` / globbed / absent grant.
pub fn owned_schemas_from_effective(effective: &EffectivePolicy) -> Vec<String> {
    let Some(k) = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok() else {
        return Vec::new();
    };
    effective
        .grant_literal_schema_includes(&k)
        .unwrap_or_default()
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
    /// The SHAPE is neutral — a set of column-name bytes and an ordered key — but
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
    match normalize_pg_identifier(name) {
        Some(object) if object.table.is_none() => object.schema,
        _ => name.to_ascii_lowercase().into_bytes(),
    }
}
