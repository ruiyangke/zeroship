//! Migration unit + supporting value types (design §2.1).
//!
//! A migration is an **immutable, ordered** artifact shipped in the `.zship`
//! bundle and recorded in the journal on apply. The version is a `UUIDv7` typed
//! id (`mig_…`) so concurrent multi-app authoring produces collision-free,
//! time-ordered versions (sequential ints collide; raw timestamps skew).

use sha2::{Digest, Sha256};
use zeroship_core::typed_id;

/// Typed-id prefix for migration versions (`mig_<base62 uuidv7>`).
///
/// Three chars to match the global `^[a-z]{3}_[A-Za-z0-9]{22}$` shape every
/// other entity uses, disjoint from every other prefix in `typed_id`.
pub const MIGRATION_PREFIX: &str = "mig";

/// Error parsing a [`MigrationId`] from a string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The id parsed but its prefix was not `mig`.
    #[error("expected prefix 'mig', got '{got}'")]
    WrongPrefix { got: String },
    /// The id was malformed (wrong shape, bad base62, missing underscore).
    #[error("malformed migration id: {0}")]
    Malformed(String),
}

/// A migration version: a `UUIDv7` typed id, `mig_<base62>`.
///
/// Time-ordered (the `UUIDv7` timestamp is in the high bits, and base62 here
/// preserves that order lexicographically), so string-sorting a set of
/// versions yields apply order — see [`MigrationId::timestamp_ms`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct MigrationId(String);

impl MigrationId {
    /// Mint a fresh, time-ordered migration id.
    #[must_use]
    pub fn generate() -> Self {
        Self(typed_id::generate(MIGRATION_PREFIX))
    }

    /// Borrow the wire string (`mig_…`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse + validate a migration id, asserting the `mig` prefix.
    ///
    /// # Errors
    /// [`IdError::WrongPrefix`] if the prefix is not `mig`; [`IdError::Malformed`]
    /// if the id does not parse (bad base62, missing underscore, wrong length).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        match typed_id::parse_with_prefix(s, MIGRATION_PREFIX) {
            Ok(_) => Ok(Self(s.to_string())),
            Err(typed_id::ParseError::WrongPrefix { got, .. }) => {
                Err(IdError::WrongPrefix { got })
            }
            Err(typed_id::ParseError::Malformed(msg)) => Err(IdError::Malformed(msg)),
        }
    }

    /// The `UUIDv7` timestamp component, in milliseconds since the Unix epoch.
    ///
    /// `UUIDv7` stores a 48-bit big-endian millisecond timestamp in its first
    /// six bytes. Two ids minted in sequence are non-decreasing here, matching
    /// their lexicographic string order.
    ///
    /// # Panics
    /// Never in practice — `self.0` is only ever a valid `mig_…` id (every
    /// constructor goes through `generate`/`parse`).
    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        let (_, uuid) = typed_id::parse(&self.0).expect("MigrationId is always a valid typed id");
        let bytes = uuid.as_bytes();
        let mut ms: u64 = 0;
        for &b in &bytes[0..6] {
            ms = (ms << 8) | u64::from(b);
        }
        ms
    }
}

/// The phase of a zero-downtime **expand-contract** online migration (design
/// §5, Plan 8). Carried only by `online` migrations (`flags.online == true`);
/// `None` for an ordinary one-shot migration.
///
/// An online column RENAME (or type change) is split across **two deploys**:
///
/// - **`Expand`** — additively grow the schema so old and new shapes coexist
///   (add the new nullable column, install a dual-write trigger, backfill).
///   Lands *before* dependent code switches over.
/// - **`Contract`** — drop the old shape once no code uses it (drop the
///   trigger + function, drop the old column). Lands *after* code switches over.
///
/// The engine enforces the split via a gate (design Plan 8 v1.2): a `Contract`
/// migration is refused unless every `Expand` migration it `depends_on` is
/// **net-applied in the journal**. This makes the journal the single source of
/// truth for the expand→contract timeline and gives cross-deploy partitioning
/// for free (a separate, later deploy can apply the contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OnlinePhase {
    /// The additive, coexistence-establishing half (add column, dual-write
    /// trigger, backfill). Lands before dependent code switches over.
    Expand,
    /// The destructive, cleanup half (drop trigger/function, drop old column).
    /// Lands after code stops using the old shape; gated on the matching
    /// `Expand` being net-applied.
    Contract,
}

/// Apply-time flags carried by a migration (design §2.1).
///
/// These four booleans are the exact §2.1 migration-unit flag set; they are
/// independent orthogonal facets (a migration can be e.g. non-transactional +
/// online + requires-approval), not a state machine, so the wire shape is a
/// flat record of bools by design. A fifth, optional `timeout_ms` lets a single
/// long migration (a large backfill, an `INDEX CONCURRENTLY` over a huge table)
/// raise its own `statement_timeout` ceiling above the executor-wide default. A
/// sixth, optional `phase` ([`OnlinePhase`]) tags an `online` expand-contract
/// step as its expand or contract half — kept as a *separate optional facet*
/// (not a fifth bool) so the bools stay orthogonal.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrationFlags {
    /// Run inside a single `BEGIN; … COMMIT` (DDL + journal atomic). Default.
    /// `false` opts into the two-phase non-transactional path (e.g.
    /// `CREATE INDEX CONCURRENTLY`).
    pub transactional: bool,
    /// Drops/truncates/lossy-type-changes — data loss. The gate (built later)
    /// decides; the guard only flags.
    pub destructive: bool,
    /// Authored as a zero-downtime expand-contract step (multi-deploy sequence).
    pub online: bool,
    /// Must be confirmed before apply (AI never auto-applies destructive).
    pub requires_approval: bool,
    /// Optional per-migration `statement_timeout`, in **milliseconds**. `None`
    /// falls back to [`crate::db::ExecutorConfig::statement_timeout`]. A long
    /// backfill or a big concurrent index sets its own higher ceiling so the
    /// conservative executor default does not kill it mid-flight.
    pub timeout_ms: Option<u64>,
    /// The expand/contract phase of an `online` migration ([`OnlinePhase`]).
    /// `None` for an ordinary one-shot migration; `Some(Expand)` /
    /// `Some(Contract)` for the two halves of a zero-downtime expand-contract
    /// sequence. Read by the engine's expand/contract gate (Plan 8 v1.2). Kept
    /// optional + separate from the four bools so they remain orthogonal facets.
    pub phase: Option<OnlinePhase>,
}

impl Default for MigrationFlags {
    fn default() -> Self {
        Self {
            transactional: true,
            destructive: false,
            online: false,
            requires_approval: false,
            timeout_ms: None,
            phase: None,
        }
    }
}

/// Tamper-evident checksum over a migration's `up` (and optional `down`) SQL.
///
/// Hex-encoded SHA-256. A mismatch on an already-applied migration is a hard
/// error (design §1.5 / §2.3 drift check). The input is **length-prefixed** so
/// `down: Some("")` and `down: None` produce *different* checksums (an empty
/// reversible down is not the same migration as an irreversible one).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checksum(String);

impl Checksum {
    /// Compute the checksum of `(up, down)`.
    #[must_use]
    pub fn of(up: &str, down: Option<&str>) -> Self {
        let mut hasher = Sha256::new();
        // Length-prefix every field with a fixed-width big-endian u64 so no
        // concatenation collision is possible (e.g. up="ab",down="c" vs
        // up="a",down="bc"), and so `down: None` (sentinel 0xFFFF…) is
        // distinct from `down: Some("")` (length 0).
        hasher.update((up.len() as u64).to_be_bytes());
        hasher.update(up.as_bytes());
        match down {
            Some(d) => {
                hasher.update((d.len() as u64).to_be_bytes());
                hasher.update(d.as_bytes());
            }
            None => {
                // Irreversibility sentinel: a length no real string can take.
                hasher.update(u64::MAX.to_be_bytes());
            }
        }
        Self(hex::encode(hasher.finalize()))
    }

    /// Borrow the hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An immutable, ordered migration artifact (design §2.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Migration {
    /// `UUIDv7` version (`mig_…`) — time-ordered, collision-free.
    pub version: MigrationId,
    /// Human-readable name, e.g. `"add_orders_table"`.
    pub name: String,
    /// The forward SQL.
    pub up: String,
    /// The reverse SQL, or `None` = explicitly irreversible (no true down).
    pub down: Option<String>,
    /// Tamper-evident checksum over `(up, down)`.
    pub checksum: Checksum,
    /// Apply-time flags.
    pub flags: MigrationFlags,
    /// The declaring app (per-table ownership) — an `app_…` typed id.
    pub owner_app: String,
    /// Optional cross-slice ordering dependencies.
    pub depends_on: Vec<MigrationId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_id_has_mig_prefix_and_roundtrips() {
        let id = MigrationId::generate();
        assert!(id.as_str().starts_with("mig_"), "got {}", id.as_str());
        // Round-trips through parse.
        let parsed = MigrationId::parse(id.as_str()).expect("generated id must parse");
        assert_eq!(parsed, id);
        // Wrong prefix is rejected (not silently accepted).
        let err = MigrationId::parse("app_0000000000000000000000").unwrap_err();
        assert!(matches!(err, IdError::WrongPrefix { .. }), "got {err:?}");
        // Malformed is rejected.
        assert!(matches!(
            MigrationId::parse("not-an-id").unwrap_err(),
            IdError::Malformed(_)
        ));
        assert!(matches!(
            MigrationId::parse("mig_short").unwrap_err(),
            IdError::Malformed(_)
        ));
    }

    #[test]
    fn migration_ids_are_time_ordered() {
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        // Timestamp is non-decreasing.
        assert!(
            b.timestamp_ms() >= a.timestamp_ms(),
            "b.ts {} should be >= a.ts {}",
            b.timestamp_ms(),
            a.timestamp_ms()
        );
        // And the 2ms gap makes it strictly greater.
        assert!(b.timestamp_ms() > a.timestamp_ms());
        // String sort matches time order (the UUIDv7 + base62 invariant).
        assert!(b.as_str() > a.as_str(), "string order must match time order");
        assert!(b > a, "Ord must match time order");
    }

    #[test]
    fn checksum_is_deterministic_and_sensitive() {
        let base = Checksum::of("CREATE TABLE t()", Some("DROP TABLE t"));
        // Deterministic.
        assert_eq!(base, Checksum::of("CREATE TABLE t()", Some("DROP TABLE t")));
        // Sensitive to `up`.
        assert_ne!(base, Checksum::of("CREATE TABLE u()", Some("DROP TABLE t")));
        // Sensitive to `down`.
        assert_ne!(base, Checksum::of("CREATE TABLE t()", Some("DROP TABLE u")));
        // `Some("")` differs from `None` (empty down != irreversible).
        assert_ne!(
            Checksum::of("CREATE TABLE t()", Some("")),
            Checksum::of("CREATE TABLE t()", None)
        );
        // And no concatenation collision: up="ab",down="c" != up="a",down="bc".
        assert_ne!(
            Checksum::of("ab", Some("c")),
            Checksum::of("a", Some("bc"))
        );
        // Hex sha256 = 64 chars.
        assert_eq!(base.as_str().len(), 64);
    }

    #[test]
    fn flags_default_is_transactional() {
        let f = MigrationFlags::default();
        assert!(f.transactional);
        assert!(!f.destructive);
        assert!(!f.online);
        assert!(!f.requires_approval);
        assert_eq!(f.phase, None);
    }
}
