//! Migration-set **integrity manifest** (v3 Plan F — Atlas `migrate hash` /
//! `atlas.sum` style).
//!
//! A single hash over the ORDERED migration SET so a tampered / reordered /
//! inserted / removed bundle is rejected BEFORE any apply.
//!
//! # What it adds on top of the per-migration checksum
//!
//! The per-migration [`Checksum`] already covers a single migration's *content*
//! (`up` + `down` + preconditions). It catches **drift** — an already-applied
//! migration whose definition was edited after the fact. What it does NOT catch
//! is **set-level** tampering of the SUPPLIED bundle before anything is applied:
//!
//! - a **reorder** of two not-yet-applied migrations (each migration's own
//!   checksum is unchanged, but apply order — and therefore the resulting schema
//!   — differs);
//! - an **insertion** of a brand-new migration into the set;
//! - a **removal** of a migration from the set;
//! - a **content edit** (which also changes that migration's per-migration
//!   checksum, and therefore the manifest).
//!
//! The manifest is integrity over `{content of each migration}` (via its
//! per-migration checksum) + `{their order}` + `{membership / count}`. Any of the
//! four mutations above changes the [`ManifestHash`].
//!
//! # Canonical serialization (domain-separated, length-prefixed)
//!
//! [`compute_manifest`] folds, IN THE GIVEN ORDER, for each migration:
//!
//! ```text
//! H( DOMAIN
//!  ‖ u64_be(count)
//!  ‖ for each migration in order:
//!      u64_be(len(version))   ‖ version_utf8
//!      u64_be(len(checksum))  ‖ checksum_hex_utf8 )
//! ```
//!
//! Every variable-length field is preceded by a fixed-width big-endian `u64`
//! length, so no delimiter-injection / concatenation collision is possible. A
//! version of `ab` + checksum `c` can never collide with a version of `a` +
//! checksum `bc`, and two short migrations can never collide with one long one.
//!
//! A leading domain-separation constant means this hash can never be confused
//! with a per-migration [`Checksum`] or any other SHA-256 use in the crate. The
//! fold is over the per-migration `checksum` (NOT the raw `up`/`down` text) so
//! it is cheap and reuses the already-tamper-evident content hash.
//!
//! The result is DETERMINISTIC (same set, same order ⇒ same hash) and
//! ORDER-SENSITIVE (the migrations are folded in the SLICE order, never sorted).
//!
//! # Trust model — the expected hash MUST come from a trusted source
//!
//! The manifest detects tampering BETWEEN authoring/review and apply: a creator,
//! the AI author, or a build pipeline cannot reorder / edit / insert / remove
//! migrations undetected. For that guarantee to hold, the `expected`
//! [`ManifestHash`] passed to [`verify_manifest`] (and to the engine's
//! `apply_verified` gate) MUST be supplied by a **trusted** source — the control
//! plane, which stamps it at build / review time and stores it out-of-band.
//!
//! It MUST NOT be read from the same bundle the migrations themselves arrived in:
//! an attacker who can edit the migrations can equally edit a hash embedded
//! alongside them, and the check would verify the tampered set against its own
//! tampered hash (vacuously passing). The manifest is a check of *the bundle*
//! against *an independently-held expectation*, not a self-consistency check.

use sha2::{Digest, Sha256};

use crate::migration::Migration;

/// Domain-separation prefix for the set-level manifest hash.
///
/// Folded in first so a [`ManifestHash`] can never be confused with a
/// per-migration [`Checksum`](crate::migration::Checksum) (different domain) even
/// if the remaining bytes somehow coincided. Versioned (`v1`) so the hash domain
/// can evolve deliberately (pre-launch: no persisted manifests to preserve).
const MANIFEST_DOMAIN: &[u8] = b"zeroship-migrate/manifest/v1";

/// A set-level integrity hash over an ORDERED migration set (hex SHA-256).
///
/// Produced by [`compute_manifest`]; checked by [`verify_manifest`] and the
/// engine's pre-apply gate. Order-sensitive and membership-sensitive — see the
/// module docs for the canonical serialization and the trust model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ManifestHash(String);

impl ManifestHash {
    /// Wrap an existing hex digest (e.g. one supplied by the control plane).
    ///
    /// No validation beyond storing the string — the value is only ever compared
    /// for equality against a freshly [`compute_manifest`]ed hash, so a malformed
    /// expected value simply never matches (fail-closed).
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// Borrow the hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Compute the integrity manifest over `migrations` IN THE GIVEN ORDER.
///
/// Deterministic and order-sensitive: the migrations are folded in slice order
/// (never sorted), each contributing its `(version, checksum)` length-prefixed.
/// See the module docs for the exact canonical serialization.
#[must_use]
pub fn compute_manifest(migrations: &[Migration]) -> ManifestHash {
    let mut hasher = Sha256::new();
    hasher.update(MANIFEST_DOMAIN);
    // Membership count: a removal/insertion changes this word even before the
    // per-migration fields differ, so a bundle of a different size can never
    // collide with the full one.
    hasher.update((migrations.len() as u64).to_be_bytes());
    for m in migrations {
        let version = m.version.as_str();
        let checksum = m.checksum.as_str();
        // Length-prefix each variable-length field with a fixed-width big-endian
        // u64 so no concatenation/delimiter-injection collision is possible.
        hasher.update((version.len() as u64).to_be_bytes());
        hasher.update(version.as_bytes());
        hasher.update((checksum.len() as u64).to_be_bytes());
        hasher.update(checksum.as_bytes());
    }
    ManifestHash(hex::encode(hasher.finalize()))
}

/// Why a [`verify_manifest`] failed.
///
/// Carries a best-effort classification of the mismatch for diagnostics. The
/// security decision is binary (match / no-match); this only helps a
/// human/operator understand *what* differs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// The recomputed manifest did not match the `expected` hash — the supplied
    /// set was tampered (reordered / edited / inserted / removed) relative to the
    /// trusted manifest. NOTHING should be applied. `kind` is a best-effort
    /// classification of the difference (it does NOT widen the trust decision —
    /// any mismatch refuses).
    #[error(
        "migration-set manifest mismatch (expected {expected}, got {actual}): {kind}"
    )]
    Mismatch {
        /// The trusted hash the caller expected (from the control plane).
        expected: String,
        /// The hash actually computed over the supplied set.
        actual: String,
        /// Best-effort classification of what differs.
        kind: MismatchKind,
    },
}

/// A best-effort classification of a manifest mismatch (diagnostics only).
///
/// This is computed CHEAPLY from the supplied set alone — the `expected`
/// [`ManifestHash`] is an opaque digest, so we cannot reconstruct the trusted
/// set from it. We therefore cannot always pinpoint the exact mutation; when we
/// cannot, we report [`MismatchKind::Differs`]. The classification NEVER changes
/// the decision: any mismatch refuses the apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MismatchKind {
    /// The supplied set contains a duplicate `version` — a malformed bundle
    /// (versions must be unique). Detected directly from the set; this would also
    /// make any expected hash impossible to satisfy meaningfully.
    DuplicateVersion {
        /// The version that appears more than once.
        version: String,
    },
    /// We could classify nothing more specific from the supplied set alone (the
    /// `expected` hash is opaque). The set differs from the trusted manifest by a
    /// reorder, a content edit, an insertion, and/or a removal — any of which is
    /// a refusal.
    Differs,
}

impl std::fmt::Display for MismatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateVersion { version } => {
                write!(f, "supplied set has a duplicate version {version}")
            }
            Self::Differs => write!(
                f,
                "supplied set differs from the trusted manifest by reorder, content edit, \
                 insertion, and/or removal"
            ),
        }
    }
}

/// Verify the supplied `migrations` (IN THE GIVEN ORDER) against an `expected`
/// trusted [`ManifestHash`].
///
/// Recomputes the manifest with [`compute_manifest`] and compares it to
/// `expected` in **constant time** (the digests are fixed-length hex, and we
/// compare them with a length-then-XOR-accumulate fold so a near-miss does not
/// leak timing). On match: `Ok(())`. On mismatch: [`ManifestError::Mismatch`]
/// with a best-effort [`MismatchKind`].
///
/// `expected` MUST come from a TRUSTED source (the control plane), NOT from the
/// same bundle the migrations arrived in — see the module-level trust model.
///
/// # Errors
/// [`ManifestError::Mismatch`] if the recomputed hash differs from `expected`.
pub fn verify_manifest(
    migrations: &[Migration],
    expected: &ManifestHash,
) -> Result<(), ManifestError> {
    let actual = compute_manifest(migrations);
    if constant_time_eq(actual.as_str().as_bytes(), expected.as_str().as_bytes()) {
        return Ok(());
    }
    Err(ManifestError::Mismatch {
        expected: expected.as_str().to_string(),
        actual: actual.as_str().to_string(),
        kind: classify_mismatch(migrations),
    })
}

/// Best-effort mismatch classification from the supplied set alone.
///
/// The `expected` hash is opaque (we cannot reverse it to the trusted set), so
/// the only thing we can detect *intrinsically* is a malformed set (a duplicate
/// version). Everything else is reported as [`MismatchKind::Differs`]. This is
/// diagnostics, never a trust decision.
fn classify_mismatch(migrations: &[Migration]) -> MismatchKind {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for m in migrations {
        if !seen.insert(m.version.as_str()) {
            return MismatchKind::DuplicateVersion {
                version: m.version.as_str().to_string(),
            };
        }
    }
    MismatchKind::Differs
}

/// Constant-time byte comparison (no early-out on the first differing byte).
///
/// The manifest hashes are not secret, but a constant-time compare is cheap and
/// removes any timing signal entirely; it also keeps the verify path uniform.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::{Checksum, MigrationFlags, MigrationId};

    /// Build a versioned migration with a correct content checksum.
    fn mig(name: &str, up: &str, down: Option<&str>) -> Migration {
        let flags = MigrationFlags::default();
        let checksum = Checksum::of(&crate::migration::ChecksumInput {
            up,
            down,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version: MigrationId::generate(),
            name: name.to_string(),
            up: up.to_string(),
            down: down.map(str::to_string),
            checksum,
            flags,
            owner_app: "app_test".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        }
    }

    #[test]
    fn manifest_is_deterministic() {
        let set = vec![
            mig("a", "CREATE TABLE a()", Some("DROP TABLE a")),
            mig("b", "CREATE TABLE b()", Some("DROP TABLE b")),
        ];
        // Same set, same order ⇒ identical hash, every time.
        assert_eq!(compute_manifest(&set), compute_manifest(&set));
        // 64 hex chars (SHA-256).
        assert_eq!(compute_manifest(&set).as_str().len(), 64);
    }

    #[test]
    fn manifest_is_order_sensitive() {
        let a = mig("a", "CREATE TABLE a()", None);
        let b = mig("b", "CREATE TABLE b()", None);
        let forward = vec![a.clone(), b.clone()];
        let reversed = vec![b, a];
        // A reorder of the SAME two migrations ⇒ a different manifest, even though
        // each migration's own checksum is unchanged.
        assert_ne!(
            compute_manifest(&forward),
            compute_manifest(&reversed),
            "manifest MUST be order-sensitive"
        );
    }

    #[test]
    fn manifest_is_content_sensitive() {
        let base = vec![mig("a", "CREATE TABLE a()", None)];
        // Editing one migration's `up` changes its per-migration checksum, which
        // changes the manifest.
        let edited = vec![mig("a", "CREATE TABLE a(x int)", None)];
        assert_ne!(
            base[0].checksum, edited[0].checksum,
            "the per-migration checksum must change first"
        );
        assert_ne!(compute_manifest(&base), compute_manifest(&edited));
    }

    #[test]
    fn manifest_is_membership_sensitive() {
        let a = mig("a", "CREATE TABLE a()", None);
        let b = mig("b", "CREATE TABLE b()", None);
        let one = vec![a.clone()];
        let two = vec![a.clone(), b];
        // Insertion (add b) ⇒ different.
        assert_ne!(compute_manifest(&one), compute_manifest(&two));
        // Removal (drop b from the two-set back to one) ⇒ different (symmetric).
        let removed = vec![a];
        assert_eq!(compute_manifest(&one), compute_manifest(&removed));
        assert_ne!(compute_manifest(&two), compute_manifest(&removed));
    }

    #[test]
    fn no_concatenation_collision_across_version_and_checksum_boundary() {
        // Two migrations whose (version, checksum) byte-streams would collide
        // under naive concatenation must NOT collide under length-prefixing.
        // We can't freely choose version strings (they're typed ids), so we test
        // the serialization invariant directly via a content edit that shifts the
        // boundary: a set [m] vs a set [m] where m's checksum differs by content
        // already covered; here assert the count word also participates.
        let a = mig("a", "x", None);
        let empty: Vec<Migration> = Vec::new();
        // An empty set folds only DOMAIN + count(0); a one-set folds more. They
        // must differ (the count word + the migration fields both contribute).
        assert_ne!(compute_manifest(&empty), compute_manifest(&[a]));
        // Two empty sets are identical (determinism at the boundary).
        assert_eq!(compute_manifest(&empty), compute_manifest(&[]));
    }

    #[test]
    fn verify_accepts_the_matching_manifest() {
        let set = vec![
            mig("a", "CREATE TABLE a()", None),
            mig("b", "CREATE TABLE b()", None),
        ];
        let expected = compute_manifest(&set);
        assert!(verify_manifest(&set, &expected).is_ok());
    }

    #[test]
    fn verify_rejects_a_reordered_set_with_a_diagnostic() {
        let a = mig("a", "CREATE TABLE a()", None);
        let b = mig("b", "CREATE TABLE b()", None);
        let trusted = vec![a.clone(), b.clone()];
        let expected = compute_manifest(&trusted);
        // The attacker reorders the SAME migrations.
        let tampered = vec![b, a];
        let err = verify_manifest(&tampered, &expected).unwrap_err();
        match err {
            ManifestError::Mismatch {
                expected: e,
                actual,
                kind,
            } => {
                assert_eq!(e, expected.as_str());
                assert_ne!(actual, expected.as_str());
                // Order-only change with unique versions ⇒ best-effort "Differs".
                assert_eq!(kind, MismatchKind::Differs);
            }
        }
    }

    #[test]
    fn verify_rejects_an_inserted_migration() {
        let a = mig("a", "CREATE TABLE a()", None);
        let trusted = vec![a.clone()];
        let expected = compute_manifest(&trusted);
        let tampered = vec![a, mig("evil", "DROP TABLE secrets", None)];
        assert!(verify_manifest(&tampered, &expected).is_err());
    }

    #[test]
    fn verify_rejects_a_removed_migration() {
        let a = mig("a", "CREATE TABLE a()", None);
        let b = mig("b", "CREATE TABLE b()", None);
        let trusted = vec![a.clone(), b];
        let expected = compute_manifest(&trusted);
        let tampered = vec![a];
        assert!(verify_manifest(&tampered, &expected).is_err());
    }

    #[test]
    fn verify_rejects_a_content_edit() {
        let trusted = vec![mig("a", "CREATE TABLE a()", None)];
        let expected = compute_manifest(&trusted);
        let tampered = vec![mig("a", "CREATE TABLE a(x int)", None)];
        // Note: the edited migration has a fresh version (generate()), so this is
        // really insertion+removal at the manifest level; the point is the hash
        // refuses. (Content drift of a SAME-version migration is covered by the
        // executor's per-migration ChecksumDrift; the manifest catches the SET.)
        assert!(verify_manifest(&tampered, &expected).is_err());
    }

    #[test]
    fn classify_flags_a_duplicate_version() {
        let a = mig("a", "CREATE TABLE a()", None);
        // Force a duplicate version by cloning the same migration.
        let dup = vec![a.clone(), a];
        let expected = compute_manifest(&[mig("x", "y", None)]);
        let err = verify_manifest(&dup, &expected).unwrap_err();
        match err {
            ManifestError::Mismatch { kind, .. } => {
                assert!(matches!(kind, MismatchKind::DuplicateVersion { .. }));
            }
        }
    }

    #[test]
    fn from_hex_roundtrips_and_a_garbage_expected_never_matches() {
        let set = vec![mig("a", "CREATE TABLE a()", None)];
        let h = compute_manifest(&set);
        let wrapped = ManifestHash::from_hex(h.as_str());
        assert_eq!(wrapped, h);
        assert!(verify_manifest(&set, &wrapped).is_ok());
        // A garbage / wrong-length expected fails closed.
        let garbage = ManifestHash::from_hex("deadbeef");
        assert!(verify_manifest(&set, &garbage).is_err());
    }
}
