//! Policy value types shared by model validation and the SQL guard.

/// A shared trust-posture label used by capability and validation vocabulary.
///
/// This enum never selects or constructs an effective policy. Guard and executor
/// APIs receive a caller-composed policy explicitly, and the host selects any
/// non-policy guard mode independently. `#[non_exhaustive]` keeps the label set
/// evolvable and requires external matches to include a wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrustProfile {
    /// Untrusted creator/AI SQL. The full deny-list (today's behaviour).
    Confined,
    /// Trusted operator SQL for the platform's own schemas. Its grants and schema
    /// allowlist come from the explicitly authored policy.
    Platform,
    /// **No untrusted boundary at all** — the public dbmate-like CLI posture
    /// where the operator owns the database. The deny-list / cross-schema /
    /// body walks are SKIPPED entirely (arbitrary SQL applies as the connecting
    /// role: `CREATE ROLE`, touch any schema, etc. — dbmate parity). The
    /// destructive/transactional/approval flags are STILL derived independently
    /// of trust, so the CLI's `--yes`
    /// data-loss gate still applies; Trusted disables the deny-list, NOT the
    /// destructive classification. A host that uses this label must still supply
    /// the policy and belt-off guard mode explicitly.
    Trusted,
}

/// The schemas a guard permits references to.
///
/// `Single` is the **Confined** shape — the `project_schema: String` semantics
/// (one allowed schema; everything else is a `CrossSchema` violation), matched
/// CASE-INSENSITIVELY. A case-variant qualifier (`'APP1'` under `'app1'`) is
/// admitted, then canonicalized to `project_schema` at render
/// (`zero_migrate::render::lower::IrAuthor::effective_schema`) so gate and render never
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
    /// That canonicalization lives in
    /// `zero_migrate::render::lower::IrAuthor::effective_schema`
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
/// restrictive: `forbid` ⊑ `warn` ⊑ `allow`. This is the enforceable
/// `sec.destructive_ops` guard posture ONLY — the guard denies/warns/allows a
/// destructive statement by it.
///
/// Approval is NOT one of these states. It is the separate, host-enforced
/// `safety.require_approval` obligation (`never`/`on_destructive`/`always`) the engine
/// only DECLARES (see [`crate::policy_registry::KEY_SAFETY_REQUIRE_APPROVAL`] +
/// [`crate::policy_approval`]); it composes independently of this posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestructiveOps {
    /// Forbid destructive operations.
    Forbid,
    /// Allow destructive operations and surface a structured warning.
    Warn,
    /// Allow destructive operations silently.
    #[default]
    Allow,
}

impl DestructiveOps {
    const fn rank(self) -> u8 {
        match self {
            Self::Forbid => 0,
            Self::Warn => 1,
            Self::Allow => 2,
        }
    }

    /// The tighter (more restrictive) of two postures (operator-charter meet).
    #[must_use]
    pub const fn tightest(self, other: Self) -> Self {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }

    /// True if `self` is looser (less restrictive) than the `charter`.
    #[must_use]
    pub const fn is_looser_than(self, charter: Self) -> bool {
        self.rank() > charter.rank()
    }
}
