//! Policy value types shared by model validation and the SQL guard.

/// The trust posture of a guard. Set at the OPERATOR CALL SITE, never derived
/// from SQL content.
///
/// The EXTERNAL trust boundary is closed by construction, but NOT by enum
/// un-nameability (`#[non_exhaustive]`
/// only forbids *exhaustive matching* and *constructing fielded variants*
/// externally; an external crate CAN still name the fieldless `Platform` /
/// `Trusted` as a value). The real external lock is that [`GuardConfig`]'s
/// fields are PRIVATE and [`GuardConfig::platform`] / [`GuardConfig::trusted`]
/// are `pub(crate)` and require a `pub(crate)` [`OperatorCapability`] token: so
/// naming `TrustProfile::Platform` / `TrustProfile::Trusted` externally is
/// *harmless* — there is no `pub` API that accepts it and no way to build a
/// privileged `GuardConfig`. Within the crate, `Platform`/`Trusted` are
/// produced ONLY inside [`GuardConfig::platform`] / [`GuardConfig::trusted`] /
/// [`crate::conn::ExecutorConfig::platform`] / [`crate::conn::ExecutorConfig::trusted`],
/// each of which REQUIRES the token (below) — so in-crate code (`submit`/`engine`)
/// cannot mint either without holding the token. `#[non_exhaustive]`
/// remains valuable: it keeps the variant set evolvable and forces external
/// matches to carry a wildcard.
///
/// [`GuardConfig`]: crate::guard::GuardConfig
/// [`GuardConfig::platform`]: crate::guard::GuardConfig::platform
/// [`GuardConfig::trusted`]: crate::guard::GuardConfig::trusted
/// [`OperatorCapability`]: crate::capability::OperatorCapability
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrustProfile {
    /// Untrusted creator/AI SQL. The full deny-list (today's behaviour).
    Confined,
    /// Trusted operator SQL for the platform's own schemas — the WIDENED
    /// deny-list (role/grant/policy/schema management admitted against a fixed
    /// schema allowlist). Constructed ONLY by `GuardConfig::platform` /
    /// `ExecutorConfig::platform`, which require an [`OperatorCapability`] token.
    ///
    /// [`OperatorCapability`]: crate::capability::OperatorCapability
    Platform,
    /// **No untrusted boundary at all** — the public dbmate-like CLI posture
    /// where the operator owns the database. The deny-list / cross-schema /
    /// body walks are SKIPPED entirely (arbitrary SQL applies as the connecting
    /// role: `CREATE ROLE`, touch any schema, etc. — dbmate parity). The
    /// destructive/transactional/approval flags are STILL derived (via
    /// `classify`/[`flags_for`], trust-independent) so the CLI's `--yes`
    /// data-loss gate still applies; Trusted disables the deny-list, NOT the
    /// destructive classification. Constructed ONLY by `GuardConfig::trusted` /
    /// `ExecutorConfig::trusted`, which require an [`OperatorCapability`] token —
    /// `submit_migration` and any external crate can NEVER reach it.
    ///
    /// [`OperatorCapability`]: crate::capability::OperatorCapability
    /// [`flags_for`]: crate::guard::flags_for
    Trusted,
}

/// The schemas a guard permits references to.
///
/// `Single` is the **Confined** shape — the `project_schema: String` semantics
/// (one allowed schema; everything else is a `CrossSchema` violation), matched
/// CASE-INSENSITIVELY. A case-variant qualifier (`'APP1'` under `'app1'`) is
/// admitted, then canonicalized to `project_schema` at render
/// ([`crate::render::lower::IrAuthor::effective_schema`]) so gate and render never
/// diverge. `Allowlist` is the **Platform** shape: a reference passes iff its
/// schema is (case-insensitively) a member of the allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaScope {
    /// Confined: exactly one permitted schema (the project schema). Any other
    /// explicitly-qualified schema is a cross-tenant violation.
    Single(String),
    /// Platform: a set of permitted schemas (e.g. a project schema / `public`). A
    /// reference is foreign iff its schema is NOT in this list.
    Allowlist(Vec<String>),
    /// Explicit Trusted/operator posture: no validate-time cross-schema
    /// confinement. This is deliberately distinct from `None` at public load /
    /// validate APIs so an omitted capability defaults to least privilege.
    Unconfined,
}

impl SchemaScope {
    /// True if `schema` is permitted by this scope (CASE-INSENSITIVE, ASCII).
    ///
    /// - `Single(s)` ⇒ `schema.eq_ignore_ascii_case(s)`.
    /// - `Allowlist(v)` ⇒ `schema` case-folds to a member of `v`.
    ///
    /// **Gate/render agreement.** The match is case-INsensitive, but
    /// the render seam (`quote_ident`) is byte-verbatim. So a
    /// case-VARIANT qualifier the gate accepts (`'APP1'` under project `'app1'`)
    /// MUST be canonicalized to `project_schema` before render, or the op would land
    /// in a DIFFERENT case-sensitive Postgres schema than the one the gate blessed.
    /// That canonicalization lives in [`crate::render::lower::IrAuthor::effective_schema`]
    /// — this `permits` only decides admission, never the rendered casing.
    #[must_use]
    pub fn permits(&self, schema: &str) -> bool {
        match self {
            Self::Single(s) => schema.eq_ignore_ascii_case(s),
            Self::Allowlist(v) => v.iter().any(|s| s.eq_ignore_ascii_case(schema)),
            Self::Unconfined => true,
        }
    }
}

/// Destructive operation posture. Ordered from more restrictive to less
/// restrictive. `RequireApproval` is retained as a server composition value, but
/// sealed engine configs accept only the enforceable `forbid`/`warn`/`allow`
/// states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestructiveOps {
    /// Forbid destructive operations.
    Forbid,
    /// Allow destructive operations and surface a structured warning.
    Warn,
    /// Require server approval before projecting to forbid/allow.
    RequireApproval,
    /// Allow today's approval-gated behavior.
    #[default]
    Allow,
}

impl DestructiveOps {
    const fn rank(self) -> u8 {
        match self {
            Self::RequireApproval => 0,
            Self::Forbid => 1,
            Self::Warn => 2,
            Self::Allow => 3,
        }
    }

    /// The tighter (more restrictive) of two postures (operator-ceiling meet).
    #[must_use]
    pub const fn tightest(self, other: Self) -> Self {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }

    /// True if `self` is looser (less restrictive) than the `ceiling`.
    #[must_use]
    pub const fn is_looser_than(self, ceiling: Self) -> bool {
        self.rank() > ceiling.rank()
    }
}
