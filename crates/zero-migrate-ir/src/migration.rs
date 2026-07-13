//! Migration unit + supporting value types.
//!
//! A migration is an **immutable, ordered** artifact shipped in the `.zship`
//! bundle and recorded in the journal on apply. The version is a `UUIDv7` typed
//! id (`mig_…`) so concurrent multi-app authoring produces collision-free,
//! time-ordered versions (sequential ints collide; raw timestamps skew).

use sha2::{Digest, Sha256};
use crate::id as typed_id;

use crate::precondition::PreconditionCheck;

/// Typed-id prefix for migration versions (`mig_<base62 uuidv7>`).
///
/// Three chars to match the global `^[a-z]{3}_[A-Za-z0-9]{22}$` shape every
/// other entity uses, disjoint from every other prefix in `typed_id`.
pub const MIGRATION_PREFIX: &str = "mig";

/// Numeric file versions fit in the high 48 bits of the deterministic UUID image.
pub const VERSION_CEILING: u64 = 1u64 << 48;

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

    /// Mint a DETERMINISTIC, STABLE migration id from a domain `tag` + a content
    /// `seed` (sub-step versioning). The id is `SHA-256(tag || seed)` laid
    /// out with the SAME high-48-bit `0xFF…FF` MARKER the loader's
    /// `repeatable_id_for_name` / the IR author's `dml_id_from_seed` use — so a
    /// derived sub-step id can **never** collide with a versioned migration id
    /// (whose high 48 bits hold a small numeric file version) and two distinct
    /// seeds collide only on an 80-bit SHA-256 prefix collision (negligible).
    ///
    /// This is the deterministic-derivation discipline mandated for every
    /// `PlanStep` sub-version (`step_id = uuidv7_derive(plan.version, step_index)`):
    /// the `ExpandContractAuthor`'s E1..C2 ids are derived from the rename's stable
    /// identity (`schema + owner + table + from + to + ty`) plus the step index, so
    /// **re-lowering the identical `.ir.json` reproduces byte-identical ids** — the
    /// property the cross-deploy obligation key, the idempotent re-run skip, the
    /// auto-discharge recognition, and the self-EXPAND exemption all depend on. A
    /// fresh `generate()` per lower (the bug this replaces) gives each deploy a
    /// different obligation key for the same logical rename, breaking all four.
    ///
    /// Deterministic (same `tag`+`seed` ⇒ same id); no OS/random/time input.
    ///
    /// # Panics
    /// Never in practice: the derived 16-byte UUID always base62-encodes to a valid
    /// `mig_…` id that [`MigrationId::parse`] accepts.
    #[must_use]
    pub fn derive(tag: &str, seed: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(tag.as_bytes());
        h.update([0u8]);
        h.update(seed);
        let digest = h.finalize();
        let mut bytes = [0u8; 16];
        // High 48 bits = the derived/repeatable MARKER (never a real file version) ⇒
        // never collides with a versioned id.
        bytes[0..6].copy_from_slice(&[0xFFu8; 6]);
        bytes[6..16].copy_from_slice(&digest[0..10]);
        let uuid = uuid::Uuid::from_bytes(bytes);
        Self::parse(&format!("mig_{}", typed_id::uuid_to_base62(&uuid)))
            .expect("derived migration id is a valid mig_ typed id")
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

/// Derive a deterministic, order-preserving [`MigrationId`] from a numeric file
/// version by placing the version in the high 48 bits of the UUID image.
#[must_use]
pub fn migration_id_for_version(version: u64) -> MigrationId {
    debug_assert!(
        version < VERSION_CEILING,
        "file version {version} exceeds the 48-bit ordering field"
    );
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&version.to_be_bytes()[2..8]);
    let uuid = uuid::Uuid::from_bytes(bytes);
    MigrationId::parse(&format!("mig_{}", typed_id::uuid_to_base62(&uuid)))
        .expect("derived id is a valid mig_ typed id")
}

/// The phase of a zero-downtime **expand-contract** online migration.
/// Carried only by `online` migrations (`flags.online == true`);
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
/// The engine enforces the split via a gate: a `Contract`
/// migration is refused unless every `Expand` migration it `depends_on` is
/// **net-applied in the journal**. This makes the journal the single source of
/// truth for the expand→contract timeline and gives cross-deploy partitioning
/// for free (a separate, later deploy can apply the contract).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum OnlinePhase {
    /// The additive, coexistence-establishing half (add column, dual-write
    /// trigger, backfill). Lands before dependent code switches over.
    Expand,
    /// The destructive, cleanup half (drop trigger/function, drop old column).
    /// Lands after code stops using the old shape; gated on the matching
    /// `Expand` being net-applied.
    Contract,
}

/// Apply-time flags carried by a migration.
///
/// These four booleans are the exact migration-unit flag set; they are
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
    /// falls back to `PgConfinement::statement_timeout`. A long
    /// backfill or a big concurrent index sets its own higher ceiling so the
    /// conservative executor default does not kill it mid-flight.
    pub timeout_ms: Option<u64>,
    /// Optional per-migration `lock_timeout`, in **milliseconds**. `None` falls
    /// back to the SHORT executor-wide default
    /// (`PgConfinement::lock_timeout`, 3s — the lock-safety
    /// envelope). This is the per-deploy maintenance-window knob: a planned
    /// migration that legitimately needs to wait longer to acquire its lock
    /// (run during a quiet window where a brief stall is acceptable) raises ONLY
    /// its own lock-acquisition budget, leaving the conservative fail-fast
    /// default in force for every other migration. It is folded into the
    /// checksum exactly like `timeout_ms`, and is bounded SHORT-by-default
    /// precisely so this override is the *only* way a migration waits longer.
    pub lock_timeout_ms: Option<u64>,
    /// The expand/contract phase of an `online` migration ([`OnlinePhase`]).
    /// `None` for an ordinary one-shot migration; `Some(Expand)` /
    /// `Some(Contract)` for the two halves of a zero-downtime expand-contract
    /// sequence. Read by the engine's expand/contract gate. Kept
    /// optional + separate from the four bools so they remain orthogonal facets.
    pub phase: Option<OnlinePhase>,
    /// **Repeatable** (Flyway `R__` / Liquibase `runOnChange`).
    /// Default `false` = an ordinary, run-once versioned migration. `true` marks
    /// a *replace-style* migration whose identity is its stable `version`/name
    /// (it is NEVER re-versioned per edit), and which **re-applies whenever its
    /// definition checksum changes** instead of running exactly once. Used for
    /// objects edited over time — views, functions, triggers — whose `up` is a
    /// `CREATE OR REPLACE …` re-run each deploy it changed.
    ///
    /// Semantics the executor enforces:
    /// - repeatables run AFTER all versioned pending migrations, ordered among
    ///   themselves by `depends_on` topo (else version order);
    /// - per repeatable, the engine reads the LATEST journaled `completed`
    ///   checksum for its identity: never-applied OR checksum-DIFFERS ⇒ re-apply
    ///   `up` + append a new `completed` event; checksum-MATCHES ⇒ SKIP;
    /// - a repeatable's *changed* checksum is **exempt** from the once-only
    ///   checksum-drift tamper-abort (a changed checksum means re-run, not abort);
    ///   a once-only migration's changed checksum STILL aborts.
    ///
    /// A repeatable's `down` is always `None` (replace-style; no true reverse).
    pub repeatable: bool,
    /// **Engine-emitted goodie DDL** — the `up` is descriptor-derived,
    /// engine-AUTHORED DDL (NOT raw creator/AI SQL) that must run under the SQLite
    /// **EngineJournal** authorizer mode rather than the confined **CreatorUp** mode.
    ///
    /// The only DDL that needs this today is the SQLite **FTS5 virtual table** (+ its
    /// sync triggers): the hardened SQLite authorizer denies `CREATE VIRTUAL TABLE …
    /// USING fts5(…)` in CreatorUp (a creator may never make a vtable) and allows it
    /// ONLY in engine mode. The FTS index is emitted by the engine from a `.fts()`
    /// descriptor — it carries no untrusted SQL string — so running it in engine mode
    /// does not widen the creator surface. `false` (default) ⇒ the historical
    /// CreatorUp-confined `up` (every ordinary CREATE TABLE / ADD COLUMN / CREATE
    /// INDEX), byte-identical to before this flag existed; the **Postgres** path never
    /// sets it (PG has no confined-creator-mode split).
    #[serde(default)]
    pub engine_goodie_ddl: bool,
}

impl Default for MigrationFlags {
    fn default() -> Self {
        Self {
            transactional: true,
            destructive: false,
            online: false,
            requires_approval: false,
            timeout_ms: None,
            lock_timeout_ms: None,
            phase: None,
            repeatable: false,
            engine_goodie_ddl: false,
        }
    }
}

/// The **apply-relevant** fields of a migration, borrowed, as the input to
/// [`Checksum::of`].
///
/// The per-migration checksum must cover the WHOLE unit the executor uses to
/// **order / partition / supersede / gate** a migration — not just its SQL
/// content. If only `(up, down, preconditions)` were hashed, a tampered bundle
/// could flip `depends_on` (reorder execution), inject `supersedes` (skip a
/// migration), flip `repeatable` (re-phase), clear `requires_approval` (un-gate),
/// or change `timeout_ms` — and still verify CLEAN against the integrity manifest
/// (which folds the per-migration checksum) AND escape the per-migration drift
/// check (which compares this checksum). So the checksum folds every field that
/// changes the effective applied set or its order.
///
/// `version` and `name` are deliberately EXCLUDED: the version is the migration's
/// IDENTITY (the journal key the checksum is compared *under*, not part of the
/// content it certifies), and `name` is a human label with no apply effect.
#[derive(Debug, Clone)]
pub struct ChecksumInput<'a> {
    /// The forward SQL.
    pub up: &'a str,
    /// The reverse SQL, or `None` = explicitly irreversible.
    pub down: Option<&'a str>,
    /// Apply-time flags — all fold in. The six bools `transactional` /
    /// `destructive` / `online` / `requires_approval` / `repeatable` /
    /// `engine_goodie_ddl`, plus the OPTIONAL FACETS `timeout_ms`
    /// (`Option<u64>`), `lock_timeout_ms` (`Option<u64>`) and `phase`
    /// (`Option<OnlinePhase>`).
    pub flags: &'a MigrationFlags,
    /// The declaring app (per-table ownership).
    pub owner_app: &'a str,
    /// Cross-slice ordering dependencies.
    pub depends_on: &'a [MigrationId],
    /// Versions this migration supersedes (squash).
    pub supersedes: &'a [MigrationId],
    /// Preconditions.
    pub preconditions: &'a [PreconditionCheck],
}

impl<'a> ChecksumInput<'a> {
    /// Build the checksum input from an assembled [`Migration`]'s apply-relevant
    /// fields (everything but `version` / `name` / the `checksum` itself). Used
    /// where a `Migration` already exists (re-derive its checksum, drift checks).
    #[must_use]
    pub fn from_migration(m: &'a Migration) -> Self {
        Self {
            up: &m.up,
            down: m.down.as_deref(),
            flags: &m.flags,
            owner_app: &m.owner_app,
            depends_on: &m.depends_on,
            supersedes: &m.supersedes,
            preconditions: &m.preconditions,
        }
    }
}

/// Tamper-evident checksum over a migration's WHOLE apply-relevant unit:
/// `up`, optional `down`, `preconditions`, **`flags`**, **`depends_on`**,
/// **`supersedes`**, and **`owner_app`**.
///
/// Hex-encoded SHA-256. A mismatch on an already-applied migration is a hard
/// error (the drift check). Every field is **length-prefixed** so
/// `down: Some("")` and `down: None` produce *different* checksums (an empty
/// reversible down is not the same migration as an irreversible one), and so no
/// concatenation collision across field boundaries is possible.
///
/// # Why these fields and not just the SQL
///
/// The executor orders by `depends_on`, partitions by `flags.repeatable`,
/// supersedes by `supersedes`, gates by `flags.requires_approval` /
/// `flags.destructive` / `flags.phase`, and times out by `flags.timeout_ms`.
/// Folding them all in means a tampered bundle that flips any of them changes
/// this checksum — so the set-level integrity manifest (which folds this
/// checksum) refuses it, and the per-migration drift check (which compares this
/// checksum on already-applied versions) flags it. Preconditions are part of the
/// migration's identity: two migrations with the same SQL but
/// different gating conditions are NOT the same migration.
///
/// # Canonical serialization
///
/// `flags` and each `precondition` are serialized to canonical JSON
/// (`serde_json`, deterministic for these plain enums/structs — no maps) and
/// length-prefixed. `depends_on` and `supersedes` are folded as ORDERED lists of
/// length-prefixed version strings, IN THE GIVEN ORDER — order is semantically
/// meaningful (a dependency list `[a, b]` is the same constraint as `[b, a]`, but
/// the manifest's canonical-executed-order fold makes any reorder of the
/// effective execution visible regardless; we keep `depends_on`/`supersedes`
/// order-as-given here so the per-migration checksum is a faithful byte image of
/// the stored vectors and a SET change is always caught). `owner_app` is folded
/// length-prefixed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checksum(String);

impl Checksum {
    /// Compute the checksum over a migration's whole apply-relevant unit.
    ///
    /// # Panics
    /// Panics only if [`MigrationFlags`] or a [`PreconditionCheck`] fails to
    /// JSON-serialize — infallible for these plain structs/enums (no maps, no
    /// non-string keys), so in practice it never panics. We `.expect` rather than
    /// swallow a failure to a default, because a silent empty serialization would
    /// collide two distinct inputs into the same security checksum.
    #[must_use]
    pub fn of(input: &ChecksumInput<'_>) -> Self {
        let mut hasher = Sha256::new();
        // up — length-prefixed with a fixed-width big-endian u64 so no
        // concatenation collision is possible (e.g. up="ab",down="c" vs
        // up="a",down="bc").
        hasher.update((input.up.len() as u64).to_be_bytes());
        hasher.update(input.up.as_bytes());
        // down — `down: None` (sentinel u64::MAX) is distinct from
        // `down: Some("")` (length 0).
        match input.down {
            Some(d) => {
                hasher.update((d.len() as u64).to_be_bytes());
                hasher.update(d.as_bytes());
            }
            None => {
                hasher.update(u64::MAX.to_be_bytes());
            }
        }
        // The common tail (flags + owner_app + depends_on + supersedes +
        // preconditions) is folded identically to `of_ir` — extracted into
        // `fold_common` so the two front doors cannot drift.
        fold_common(
            &mut hasher,
            input.flags,
            input.owner_app,
            input.depends_on,
            input.supersedes,
            input.preconditions,
        );
        Self(hex::encode(hasher.finalize()))
    }

    /// Compute the checksum over a migration authored in the `op.*` IR — the
    /// canonical op-list region in PLACE OF the `up`/`down`
    /// region, then the SAME [`fold_common`] tail as [`Checksum::of`].
    ///
    /// The op-list region is [`crate::model::ir::CanonicalOpList::canonical_bytes`]: an op count,
    /// then each `Op`'s RFC 8785 (JCS) bytes length-prefixed in op order — so a
    /// reorder/insert, an `Insert` row scalar change, or a change to an embedded
    /// expression-AST `Literal` (all fold, since they live inside the op value)
    /// is drift. The region is folded as ONE length-prefixed blob so it is
    /// domain-separated from the `of` up/down region (an IR migration and a
    /// rendered-SQL migration with the same common tail get distinct checksums).
    ///
    /// Scope of JCS = the op-list region ONLY; the `fold_common` tail keeps the
    /// existing serde discipline, NOT JCS.
    ///
    /// # The `flags` argument MUST be dialect-NEUTRAL
    ///
    /// `of_ir` is dialect-neutral BY CONSTRUCTION — it takes no dialect parameter
    /// and hashes only the neutral op list + the derived+overridden flags +
    /// owner + deps + preconditions. A single portable migration therefore has
    /// ONE checksum across the PG and SQLite renders (the single-artifact /
    /// single-checksum invariant).
    ///
    /// A future `IrAuthor` MUST pass the **dialect-neutral derived-then-overridden**
    /// flags here — NEVER the per-dialect *lowered* flags. The lowering legitimately
    /// diverges per dialect (e.g. SQLite forces `transactional: true` and drops
    /// `concurrently` for a concurrent index while PG keeps `transactional: false`).
    /// Folding those POST-lowering per-dialect flags into the
    /// hash would make `of_ir` diverge per dialect and silently break the
    /// single-checksum invariant. The `transactional`/`concurrently` divergence is
    /// a render-time concern; it does not belong in the identity checksum.
    #[must_use]
    pub fn of_ir(
        ops: &crate::ir::CanonicalOpList<'_>,
        flags: &MigrationFlags,
        owner_app: &str,
        depends_on: &[MigrationId],
        supersedes: &[MigrationId],
        preconditions: &[PreconditionCheck],
    ) -> Self {
        let mut hasher = Sha256::new();
        // Explicit domain tag — an IR migration's checksum is provably
        // non-colliding with a rendered-SQL migration's (`Checksum::of`)
        // REGARDLESS of any future field addition to either front door. `of`
        // carries NO tag (its byte output is frozen by a golden fixture and by
        // every stored declarative-path checksum), so this one-sided tag on the
        // brand-new `of_ir` (no persisted checksums yet) is the safe way to add
        // the separation without drifting `of`. Folded length-prefixed like
        // every other field so it cannot run together with the region.
        const IR_DOMAIN_TAG: &[u8] = b"zero-migrate/of_ir/v1";
        hasher.update((IR_DOMAIN_TAG.len() as u64).to_be_bytes());
        hasher.update(IR_DOMAIN_TAG);
        // op-list region — the canonical bytes folded as one length-prefixed
        // blob (domain-separated from the up/down region of `of`).
        let region = ops.canonical_bytes();
        hasher.update((region.len() as u64).to_be_bytes());
        hasher.update(&region);
        // …then the SAME common tail.
        fold_common(
            &mut hasher,
            flags,
            owner_app,
            depends_on,
            supersedes,
            preconditions,
        );
        Self(hex::encode(hasher.finalize()))
    }

    /// Borrow the hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fold the **common tail** shared by [`Checksum::of`] and [`Checksum::of_ir`]:
/// `flags` (canonical JSON, length-prefixed) + `owner_app` (length-prefixed) +
/// `depends_on` + `supersedes` (each an ordered, domain-separated version list)
/// + `preconditions` (count, then each canonical-JSON + length-prefixed).
///
/// This is a PURE lift of the tail that used to live inline in `Checksum::of`;
/// extracting it guarantees the two front doors fold the identity fields
/// byte-identically (a drift between them would be a tamper-evidence hole).
///
/// # Panics
/// Panics only if [`MigrationFlags`] or a [`PreconditionCheck`] fails to
/// JSON-serialize — infallible for these plain structs/enums (no maps), so in
/// practice never. We `.expect` rather than swallow to a default: a silent empty
/// serialization would collide two distinct inputs into the same checksum.
fn fold_common(
    hasher: &mut Sha256,
    flags: &MigrationFlags,
    owner_app: &str,
    depends_on: &[MigrationId],
    supersedes: &[MigrationId],
    preconditions: &[PreconditionCheck],
) {
    // flags — canonical JSON, length-prefixed. Covers transactional /
    // destructive / online / requires_approval / timeout_ms / lock_timeout_ms /
    // phase / repeatable / engine_goodie_ddl in one deterministic image, so any
    // flip changes the hash (an attacker cannot silently inflate the
    // lock-acquisition budget past the fail-fast default without tripping the
    // drift check).
    let flags_json =
        serde_json::to_string(flags).expect("MigrationFlags is infallibly serializable");
    hasher.update((flags_json.len() as u64).to_be_bytes());
    hasher.update(flags_json.as_bytes());
    // owner_app — length-prefixed.
    hasher.update((owner_app.len() as u64).to_be_bytes());
    hasher.update(owner_app.as_bytes());
    // depends_on — ordered list: count, then each version string length-prefixed
    // in the GIVEN order (a reorder or set change shifts the hash). Domain-
    // separated from supersedes by being folded first with its own count word,
    // so a dep `[a]` + supersedes `[]` can never collide with a dep `[]` +
    // supersedes `[a]`.
    fold_version_list(hasher, depends_on);
    // supersedes — same ordered-list discipline.
    fold_version_list(hasher, supersedes);
    // preconditions: count, then each canonical-JSON-serialized + length-
    // prefixed. An empty list folds a 0 count and contributes nothing else.
    hasher.update((preconditions.len() as u64).to_be_bytes());
    for pc in preconditions {
        // `.expect` (not `.unwrap_or_default()`): a PreconditionCheck is
        // infallibly serializable; if it ever did fail, an empty string would
        // silently collide two DISTINCT preconditions into the SAME checksum
        // (a tamper-evidence hole). Fail loud instead.
        let json =
            serde_json::to_string(pc).expect("PreconditionCheck is infallibly serializable");
        hasher.update((json.len() as u64).to_be_bytes());
        hasher.update(json.as_bytes());
    }
}

/// Fold an ORDERED list of migration-id version strings into `hasher`: a
/// fixed-width big-endian u64 count, then each version string length-prefixed in
/// the GIVEN order. The count word means an empty list is distinct from a
/// one-element list and no two adjacent lists can run together.
fn fold_version_list(hasher: &mut Sha256, versions: &[MigrationId]) {
    hasher.update((versions.len() as u64).to_be_bytes());
    for v in versions {
        let s = v.as_str();
        hasher.update((s.len() as u64).to_be_bytes());
        hasher.update(s.as_bytes());
    }
}

/// An immutable, ordered migration artifact.
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
    /// The versions this migration **supersedes** (squash). Empty for an
    /// ordinary migration; non-empty only for a **squash** migration `S` that
    /// collapses a contiguous prefix `[v1..vN]` of applied history into a single
    /// equivalent step. `S.up` is the combined DDL of `[v1..vN]` (so a fresh
    /// rebuild can run it), and `supersedes = [v1..vN]`.
    ///
    /// Supersession makes the journal append-only-safe (the old `v1..vN` events
    /// are NEVER deleted): a version `v_i` is considered **satisfied** by either
    /// `v_i` itself being net-applied OR a squash `S` that supersedes `v_i` being
    /// net-applied. The executor's pending computation honors this, so on a fresh
    /// DB applying `S` runs `S.up` once and the superseded `v1..vN` are skipped
    /// (never double-applied), while on an existing DB that already ran `v1..vN`
    /// the squash is recorded WITHOUT running `S.up` (see `the squash op`).
    #[serde(default)]
    pub supersedes: Vec<MigrationId>,
    /// Optional **preconditions**: assertions evaluated against the
    /// live DB BEFORE this migration's `up` runs, gating whether it applies.
    /// Empty (the default) = unconditional apply. Each [`PreconditionCheck`]
    /// carries an assertion ([`Precondition`](crate::model::precondition::Precondition))
    /// and an unmet policy ([`OnUnmet`](crate::model::precondition::OnUnmet)): `Halt`
    /// (fail-closed — abort the apply, nothing applied) or `Skip` (leave this
    /// migration pending, re-evaluate next deploy). Folded into [`checksum`] so a
    /// precondition change is drift, exactly like an SQL change.
    ///
    /// [`checksum`]: Migration::checksum
    #[serde(default)]
    pub preconditions: Vec<PreconditionCheck>,
    /// The executor-side existence-guard probe, stamped
    /// onto this migration at IR-lower time when its source op carried an
    /// `existence_guard` (`ifNotExists`/`ifExists`). At apply time the executor
    /// reads the live catalog under the held project advisory lock + the open
    /// per-step transaction and `existence_probe::decide`s whether to
    /// run the `up` bare, journal a satisfied no-op (skip the `up`), or fail closed
    /// on a shape divergence. `None` for an unguarded migration and for every
    /// `.sql`-path / declarative migration.
    ///
    /// DELIBERATELY EXCLUDED from [`ChecksumInput`] / [`Checksum::of`]: the IR-path
    /// drift anchor is [`Checksum::of_ir`] over the op-list, which ALREADY folds the
    /// guard, and the `.sql` path never sets this field. Folding it into the
    /// per-migration checksum would change every existing golden/checksum (the field
    /// defaults to `None`); excluding it keeps them byte-identical. The field is
    /// `skip_serializing_if = "Option::is_none"` so the on-disk wire is unchanged
    /// when unset; it round-trips only the in-memory plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existence_guard: Option<crate::probe::GuardProbe>,
}

impl Migration {
    /// Recompute and store this migration's [`checksum`](Migration::checksum)
    /// from its CURRENT apply-relevant fields (`up` / `down` / `flags` /
    /// `owner_app` / `depends_on` / `supersedes` / `preconditions`).
    ///
    /// Use after editing any of those fields on an in-memory migration so the
    /// stored checksum stays a faithful image of the unit (otherwise the
    /// set-level manifest and the per-migration drift check would correctly see a
    /// changed unit). The authoring/declarative generators set the checksum at
    /// construction; this is the re-derive seam.
    pub fn recompute_checksum(&mut self) {
        self.checksum = Checksum::of(&ChecksumInput::from_migration(self));
    }
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

    /// A content-only checksum input (default flags, no deps/supersedes,
    /// `owner_app` `app_test`) — the common shape the older 3-arg `Checksum::of`
    /// covered. Field-specific tests below override one field at a time.
    fn input<'a>(
        up: &'a str,
        down: Option<&'a str>,
        flags: &'a MigrationFlags,
        owner_app: &'a str,
        depends_on: &'a [MigrationId],
        supersedes: &'a [MigrationId],
        preconditions: &'a [PreconditionCheck],
    ) -> ChecksumInput<'a> {
        ChecksumInput {
            up,
            down,
            flags,
            owner_app,
            depends_on,
            supersedes,
            preconditions,
        }
    }

    #[test]
    fn checksum_is_deterministic_and_sensitive() {
        use crate::precondition::{Precondition, PreconditionCheck};
        let f = MigrationFlags::default();
        let base = Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &[]));
        // Deterministic.
        assert_eq!(base, Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &[])));
        // Sensitive to `up`.
        assert_ne!(base, Checksum::of(&input("CREATE TABLE u()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &[])));
        // Sensitive to `down`.
        assert_ne!(base, Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE u"), &f, "app_test", &[], &[], &[])));
        // `Some("")` differs from `None` (empty down != irreversible).
        assert_ne!(
            Checksum::of(&input("CREATE TABLE t()", Some(""), &f, "app_test", &[], &[], &[])),
            Checksum::of(&input("CREATE TABLE t()", None, &f, "app_test", &[], &[], &[]))
        );
        // And no concatenation collision: up="ab",down="c" != up="a",down="bc".
        assert_ne!(
            Checksum::of(&input("ab", Some("c"), &f, "app_test", &[], &[], &[])),
            Checksum::of(&input("a", Some("bc"), &f, "app_test", &[], &[], &[]))
        );
        // Sensitive to PRECONDITIONS: same SQL, different gating condition =>
        // different checksum (preconditions are part of the migration identity).
        let pre = [PreconditionCheck::halt(Precondition::TableExists {
            table: "users".to_string(),
        })];
        assert_ne!(base, Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre)));
        // Deterministic with preconditions.
        assert_eq!(
            Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre)),
            Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre))
        );
        // A DIFFERENT precondition => a different checksum.
        let pre2 = [PreconditionCheck::halt(Precondition::TableNotExists {
            table: "users".to_string(),
        })];
        assert_ne!(
            Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre)),
            Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre2))
        );
        // A different unmet policy on the SAME check => a different checksum.
        let pre_skip = [PreconditionCheck::skip(Precondition::TableExists {
            table: "users".to_string(),
        })];
        assert_ne!(
            Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre)),
            Checksum::of(&input("CREATE TABLE t()", Some("DROP TABLE t"), &f, "app_test", &[], &[], &pre_skip))
        );
        // Hex sha256 = 64 chars.
        assert_eq!(base.as_str().len(), 64);
    }

    /// The per-migration checksum covers the WHOLE apply-relevant
    /// unit, so a tampered `flags` / `depends_on` / `supersedes` / `owner_app`
    /// changes the checksum (and therefore the integrity manifest + the drift
    /// check). RED before the `Checksum::of` widening (those fields were unhashed).
    #[test]
    fn checksum_covers_flags_deps_supersedes_owner() {
        let up = "CREATE TABLE t()";
        let owner = "app_alpha";
        let base_flags = MigrationFlags::default();
        let base = Checksum::of(&input(up, None, &base_flags, owner, &[], &[], &[]));

        // --- flags: each apply-relevant field flips the checksum ---
        // repeatable flip (re-phase: repeatables run after versioned migrations).
        let f_rep = MigrationFlags { repeatable: true, ..MigrationFlags::default() };
        assert_ne!(base, Checksum::of(&input(up, None, &f_rep, owner, &[], &[], &[])), "repeatable flip must change the checksum");
        // requires_approval clear (un-gate). Default is false; set true here so
        // "clearing" it (back to default) differs from the gated form.
        let f_appr = MigrationFlags { requires_approval: true, ..MigrationFlags::default() };
        assert_ne!(base, Checksum::of(&input(up, None, &f_appr, owner, &[], &[], &[])), "requires_approval change must change the checksum");
        // timeout_ms change.
        let f_to = MigrationFlags { timeout_ms: Some(60_000), ..MigrationFlags::default() };
        assert_ne!(base, Checksum::of(&input(up, None, &f_to, owner, &[], &[], &[])), "timeout_ms change must change the checksum");
        // lock_timeout_ms change — the per-deploy maintenance-window override
        // folds into the tamper-evident checksum exactly like timeout_ms, so an
        // attacker cannot silently inflate the lock-acquisition budget past the
        // SHORT fail-fast default without tripping the drift check.
        let f_lto = MigrationFlags { lock_timeout_ms: Some(30_000), ..MigrationFlags::default() };
        assert_ne!(base, Checksum::of(&input(up, None, &f_lto, owner, &[], &[], &[])), "lock_timeout_ms change must change the checksum");
        // destructive flip.
        let f_destr = MigrationFlags { destructive: true, ..MigrationFlags::default() };
        assert_ne!(base, Checksum::of(&input(up, None, &f_destr, owner, &[], &[], &[])), "destructive flip must change the checksum");
        // online + phase.
        let f_phase = MigrationFlags { online: true, phase: Some(OnlinePhase::Contract), ..MigrationFlags::default() };
        assert_ne!(base, Checksum::of(&input(up, None, &f_phase, owner, &[], &[], &[])), "phase change must change the checksum");

        // --- depends_on: an inserted dependency edge flips the checksum ---
        let deps = [MigrationId::generate()];
        assert_ne!(base, Checksum::of(&input(up, None, &base_flags, owner, &deps, &[], &[])), "depends_on edit must change the checksum");

        // --- supersedes: an injected supersession flips the checksum ---
        let sup = MigrationId::generate();
        let sups = [sup];
        assert_ne!(base, Checksum::of(&input(up, None, &base_flags, owner, &[], &sups, &[])), "supersedes injection must change the checksum");

        // --- owner_app: a re-owned migration flips the checksum ---
        assert_ne!(base, Checksum::of(&input(up, None, &base_flags, "app_beta", &[], &[], &[])), "owner_app change must change the checksum");

        // depends_on and supersedes are domain-separated: dep=[x],sup=[] !=
        // dep=[],sup=[x] (no cross-list concatenation collision).
        let x = MigrationId::generate();
        let xs = [x];
        assert_ne!(
            Checksum::of(&input(up, None, &base_flags, owner, &xs, &[], &[])),
            Checksum::of(&input(up, None, &base_flags, owner, &[], &xs, &[])),
            "depends_on and supersedes must be domain-separated"
        );
    }

    /// `ChecksumInput::from_migration` re-derives exactly the stored checksum.
    #[test]
    fn from_migration_redrives_checksum() {
        let f = MigrationFlags { destructive: true, ..MigrationFlags::default() };
        let up = "CREATE TABLE t()";
        let expected = Checksum::of(&input(up, Some("DROP TABLE t"), &f, "app_z", &[], &[], &[]));
        let m = Migration {
            version: MigrationId::generate(),
            name: "t".to_string(),
            up: up.to_string(),
            down: Some("DROP TABLE t".to_string()),
            checksum: expected.clone(),
            flags: f,
            owner_app: "app_z".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        };
        assert_eq!(Checksum::of(&ChecksumInput::from_migration(&m)), expected);
    }

    #[test]
    fn flags_default_is_transactional() {
        let f = MigrationFlags::default();
        assert!(f.transactional);
        assert!(!f.destructive);
        assert!(!f.online);
        assert!(!f.requires_approval);
        assert_eq!(f.phase, None);
        assert!(!f.repeatable);
    }
}
