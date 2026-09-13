//! The ONE place an app id turns into something else.
//!
//! # Why this module exists
//!
//! App identity seeds database names, roles, routing keys, and other scopes.
//! Keeping those derivations together makes each use of its spelling explicit.
//!
//! Every function here takes an [`AppId`] and returns one derived identifier.
//! That makes the derivations enumerable, gives each one a name a reviewer can
//! grep for, and - most importantly - makes them all change together.
//!
//! # Every one of these is the printed id, and the spelling is pinned
//!
//! There is one rendering of an app id, so most of these functions are the
//! identity on [`AppId::as_str`] and the rest wrap it in a brace, a slash or a
//! prefix. That is not redundancy: a bare `app_id.as_str()` says nothing about
//! which of the ten meanings was wanted, and the whole point is that the day
//! they diverge the compiler visits each one.
//!
//! `tests::golden_vectors` pins each function against a frozen literal AND,
//! where a pre-seam `&str` composer still exists, against that composer, so a
//! change of spelling fails here rather than in production. Some of those bytes
//! name objects `PostgreSQL` will not reclaim - see [`publication_name`] - and
//! one of them is an encryption salt whose silent half is worse than its loud
//! half - see [`encryption_salt`].
//!
//! [`ring_key`] carried the embedded hundred and twenty eight bits until the id
//! became text, so that a change of printed form could not rehash the ring and
//! evict every warm isolate at once. Nothing persists a ring position: the
//! gateway rebuilds its `BTreeMap` from the roster on every sync, so the only
//! cost of a re-key is the eviction itself, taken once, when the ids change.
//! It stays a named derivation because `worker_ring` takes bytes and a caller
//! must say which bytes those are.
//!
//! # What is NOT here yet
//!
//! The seam is typed on [`AppId`], and much of the tree still carries an app id
//! as `&str` - `zeroship_data_orm::encryption::keys::resolve`,
//! `zeroship_data_v8::replication`, `zeroship_kv::backend::scope`,
//! `zeroship_storage::backend`. Those sites cannot construct an
//! [`AppId`] without a fallible parse that would refuse the non-uuid app ids
//! their own tests pass, so they keep their present composers until the string
//! is typed out of them. [`crate::database_role::per_app_role_name`] and
//! [`crate::replication_names::publication_name`] survive for exactly that
//! reason, and for one more: they are the second oracle the golden vectors here
//! are compared against, so this module and the untyped composers cannot drift
//! apart while both exist. They go when the last `&str` caller does.

use sha2::{Digest, Sha256};

use crate::app_id::AppId;
use crate::database_role::{self, PerAppRoleNameError};
use crate::replication_names::{self, OBJECT_PREFIX};

/// The seed prefix every app-lifecycle advisory lock hashes.
///
/// **This constant is the whole of silent-breakage class two.** The seed was
/// hand-spelled in four production statements - the workflow claim, the
/// rollout admission check, and archive and unarchive in the control registry.
/// Three of them are the SHARED side of the lock and one is the EXCLUSIVE side,
/// so a typo in any single one does not fail: the two sides simply stop
/// contending, the archive marker stops being a fence, and a claim commits
/// across it. Spelled once, that cannot happen.
///
/// `tests::the_lifecycle_lock_seed_is_spelled_in_exactly_one_production_file`
/// keeps a fifth hand-spelling from appearing.
pub const LIFECYCLE_LOCK_SEED_PREFIX: &str = "zeroship:app-lifecycle:";

/// Why a derived identifier could not be composed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DerivationError {
    /// The complete per-app role name does not fit in a `PostgreSQL`
    /// identifier. Carries the underlying refusal unchanged; the composer
    /// never truncates or hashes an authorization role.
    #[error(transparent)]
    RoleName(#[from] PerAppRoleNameError),
    /// A replication slot is named for an (app, worker) pair and the worker
    /// half was empty, which would make two workers share one slot - and a
    /// logical slot admits exactly one consumer.
    #[error("replication: worker_id must not be empty")]
    EmptyWorkerId,
}

/// The physical `PostgreSQL` schema this app's tables live in.
///
/// Today the schema IS the tenant string, which is why
/// `zeroship_migrate_server::apply::apply_ir_documents` could write
/// `app_id.to_string()` and be right. Its comment there marks that line as THE
/// service's one app-id-to-schema derivation and says the new derivation lands
/// on it; this is that derivation, hoisted so the data plane and the migration
/// service cannot answer the question differently.
///
/// The caller validates the derived spelling with [`crate::schema_name::SchemaName`].
#[must_use]
pub fn schema_name(app: &AppId) -> String {
    app.as_str().to_owned()
}

/// The per-app `PostgreSQL` role the runtime narrows to with `SET LOCAL ROLE`.
///
/// # Errors
///
/// [`DerivationError::RoleName`] if the complete name exceeds `PostgreSQL`'s
/// identifier limit. It is refused rather than shortened, because a truncated
/// authorization role can collide with a different app's.
pub fn role_name(app: &AppId) -> Result<String, DerivationError> {
    Ok(database_role::per_app_role_name(app.as_str())?)
}

/// The app's logical-replication publication.
///
/// Infallible here where the pre-seam composer was not: its two refusals are an
/// empty id and an embedded NUL, and an [`AppId`] can be neither.
///
/// **Breakage class three has its only typed producer here.** Renaming this
/// renames every publication and leaks the old ones, which are cluster-wide
/// objects; nothing in `PostgreSQL` reclaims them and nothing in the tree
/// notices.
#[must_use]
pub fn publication_name(app: &AppId) -> String {
    replication_names::publication_name(app.as_str())
        .expect("an AppId is neither empty nor NUL-bearing")
}

/// This worker's replication slot for this app.
///
/// Every worker needs its own slot because a logical slot admits exactly one
/// consumer; the app half and the worker half are separate hash tokens so the
/// whole name stays inside `PostgreSQL`'s identifier limit.
///
/// # Errors
///
/// [`DerivationError::EmptyWorkerId`] if `worker_id` is empty.
pub fn worker_slot_name(app: &AppId, worker_id: &str) -> Result<String, DerivationError> {
    if worker_id.is_empty() {
        return Err(DerivationError::EmptyWorkerId);
    }
    Ok(format!(
        "{}{}",
        worker_slot_name_prefix(app),
        stable_token(worker_id, 10)
    ))
}

/// The exact leading substring shared by every one of this app's worker slots.
///
/// Queries compare it with `left(slot_name, length($1)) = $1`, so it must be a
/// literal prefix and never a pattern: no app-controlled wildcard can broaden
/// the match.
#[must_use]
pub fn worker_slot_name_prefix(app: &AppId) -> String {
    format!("{OBJECT_PREFIX}slot_{}__", stable_token(app.as_str(), 14))
}

/// The HKDF salt both per-app column keys are derived from.
///
/// **Changing what this returns is not a migration.** `k_enc` and `k_siv` are
/// expanded from the root key salted by these bytes, so a new salt yields new
/// keys. The `k_enc` half fails loudly - existing ciphertext no longer decrypts.
/// The `k_siv` half fails SILENTLY and is the worse of the two: deterministic
/// lookup tokens stop matching, so an equality search over an encrypted column
/// returns FEWER ROWS AND NO ERROR. Pre-launch the answer is to drop and
/// recreate every encrypted column, not to migrate it.
#[must_use]
pub fn encryption_salt(app: &AppId) -> &[u8] {
    app.as_str().as_bytes()
}

/// The `SQLite` `ATTACH` alias the dev tier opens this app's file under.
///
/// The alias is also what the preupdate hook reports as `db_name`, so the CDC
/// publisher routes changes by it: it is a routing key as much as a handle.
#[must_use]
pub fn attach_alias(app: &AppId) -> String {
    app.as_str().to_owned()
}

/// The key this app's usage counters are held under in the worker's meter.
///
/// **Breakage class one lives on the other side of this key.**
/// `zeroship_metering::meter::Meter::drain` parses it back with
/// `Uuid::parse_str` and, on failure, SKIPS AND EVICTS the counters behind a
/// `tracing::warn!`. An app whose key stops parsing therefore serves traffic
/// and is never billed, and the log noise decays by design because the counters
/// are dropped rather than retried.
#[must_use]
pub fn meter_key(app: &AppId) -> String {
    app.as_str().to_owned()
}

/// The bytes the consistent-hash ring places this app by.
///
/// `zeroship_core::worker_ring` takes bytes rather than an [`AppId`] so it stays
/// free of the id types; this is the function that says which bytes, and it is
/// the only caller-visible answer to that question.
///
/// Changing what this returns rehashes the whole ring and evicts every warm
/// isolate in the fleet. It costs a cold start per app and nothing else - no
/// ring position is stored anywhere, so there is no state left behind
/// disagreeing with the new one.
#[must_use]
pub fn ring_key(app: &AppId) -> &[u8] {
    app.as_str().as_bytes()
}

/// The text every app-lifecycle advisory lock hashes to a key.
///
/// The four production statements bind this and hash it with
/// `hashtextextended($1, 0)`; `PostgreSQL` infers `$1` as `text` from that
/// function's only candidate signature. They composed the seed in SQL - from
/// `($1::uuid)::text` - until it was hoisted here, which is why the constant
/// above insists on one spelling: a lock nobody contends for looks exactly like
/// a lock nobody wanted.
///
/// The key is a hash of this text, so it moves whenever the printed id does.
/// That is safe only because an advisory lock is session state and not a stored
/// value: no lock outlives the process holding it, so there is no old key for a
/// new one to fail to contend with.
#[must_use]
pub fn lifecycle_lock_seed(app: &AppId) -> String {
    format!("{LIFECYCLE_LOCK_SEED_PREFIX}{}", app.as_str())
}

/// The path segment this app's bundle and assets hang off in the blob store.
///
/// `LocalFs` composes `<base>/<segment>/bundle.appbundle` and
/// `<base>/<segment>/assets/<path>`; the manifest store composes
/// `<root>/manifests/<segment>/<deploy_hash>.json`.
#[must_use]
pub fn bundle_path_segment(app: &AppId) -> String {
    app.as_str().to_owned()
}

/// The Redis cluster hash tag every one of this app's KV keys carries.
///
/// `zeroship_kv::backend::scope` composes `<scope>:<user key>`. The
/// braces are the tag, and they are load-bearing twice over: they keep one
/// app's whole keyspace on one shard, and `validate_key` refuses a user key
/// containing a brace precisely so a key cannot forge a second tag and escape
/// its app's slot.
#[must_use]
pub fn kv_scope(app: &AppId) -> String {
    format!("{{{}}}", app.as_str())
}

/// The object-key prefix this app owns in the storage backend.
///
/// The kernel enforces `<app_id>/<bucket>/<key>`, so this is both the write
/// namespace and the prefix a list is bounded by.
#[must_use]
pub fn storage_prefix(app: &AppId) -> String {
    format!("{}/", app.as_str())
}

/// The first `bytes` of SHA-256 over `value`, as lowercase hexadecimal.
fn stable_token(value: &str, bytes: usize) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut token = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The app-id fixture the rest of these tests derive from.
    ///
    /// It is the canonical rendering of `0191e7a2-b3c4-4d5e-8f90-123456789abc`,
    /// the uuid this crate used as its app-id fixture while the id was a uuid,
    /// so a reader comparing this file against its history is looking at one
    /// app throughout and not two.
    const FIXTURE: &str = "app_03cgepu94hyemwpcipafo7264";

    fn fixture() -> AppId {
        AppId::parse(FIXTURE).expect("the fixture is a canonical app id")
    }

    /// Every derived identifier, pinned to its bytes.
    ///
    /// TWO ORACLES PER LINE WHERE ONE EXISTS. The frozen literal catches this
    /// composer and the `&str` composer it wraps drifting together; the
    /// comparison against that composer catches the literal being updated to
    /// match a mistake. Where no `&str` composer exists - the site inlines the
    /// derivation - the literal is the only oracle and says so.
    ///
    /// MUTATION-CHECKED: altering any one composer fails this test and nothing
    /// else in the crate.
    #[test]
    fn golden_vectors() {
        let app = fixture();

        // -- Derivations with a named `&str` composer. Literal AND the twin. --

        assert_eq!(
            role_name(&app).expect("the fixture role name fits"),
            "app_app_03cgepu94hyemwpcipafo7264_role"
        );
        assert_eq!(
            role_name(&app).expect("the fixture role name fits"),
            database_role::per_app_role_name(FIXTURE).expect("untyped role name"),
            "the seam must compose the role the migration service provisions"
        );

        assert_eq!(
            publication_name(&app),
            "__zs_pub_2b19d2d9cc47ffdd41163308916b"
        );
        assert_eq!(
            publication_name(&app),
            replication_names::publication_name(FIXTURE).expect("untyped publication name")
        );

        // -- Derivations the site inlines. The literal is the only oracle. ----

        // The migration service's app-id-to-schema step.
        assert_eq!(schema_name(&app), FIXTURE);

        // `replication::worker_slot_name`, which lives behind a `&str` API in
        // `zeroship-data-v8` that this crate must not depend on. The
        // differential against it is in that crate, next to the function.
        assert_eq!(
            worker_slot_name(&app, "worker-a").expect("the fixture slot name composes"),
            "__zs_slot_2b19d2d9cc47ffdd41163308916b__6a65e237ae44c42895b5"
        );
        assert_eq!(
            worker_slot_name_prefix(&app),
            "__zs_slot_2b19d2d9cc47ffdd41163308916b__"
        );
        assert!(
            worker_slot_name(&app, "worker-a")
                .expect("composes")
                .starts_with(&worker_slot_name_prefix(&app)),
            "the prefix must be a literal prefix of the slot, or `left(slot, n)` \
             matching stops selecting this app's slots"
        );

        // The HKDF salt `derive_key` expands both per-app column keys from.
        assert_eq!(encryption_salt(&app), FIXTURE.as_bytes());

        // The ATTACH alias, the meter key and the blob path segment are all the
        // bare tenant string at their sites.
        assert_eq!(attach_alias(&app), FIXTURE);
        assert_eq!(meter_key(&app), FIXTURE);
        assert_eq!(bundle_path_segment(&app), FIXTURE);

        // `backend::scope` wraps the id in Redis hash-tag braces.
        assert_eq!(kv_scope(&app), "{app_03cgepu94hyemwpcipafo7264}");

        // The storage kernel enforces `<app_id>/<bucket>/<key>`.
        assert_eq!(storage_prefix(&app), "app_03cgepu94hyemwpcipafo7264/");

        // The text the four advisory-lock statements bind.
        assert_eq!(
            lifecycle_lock_seed(&app),
            "zeroship:app-lifecycle:app_03cgepu94hyemwpcipafo7264"
        );

        // The ring hashes the printed id. It held the embedded uuid bits until
        // the id became text; nothing stores a ring position, so the re-key
        // costs a cold start and leaves no stale state behind.
        assert_eq!(ring_key(&app), FIXTURE.as_bytes());
    }

    /// Every derivation is a function OF THE ID, not of a constant.
    ///
    /// The golden vectors above drive one app, so a composer that ignored its
    /// argument and returned its own frozen literal would pass all of them.
    /// This is the arm that refuses that: two distinct ids must disagree
    /// everywhere, including the two hashed derivations, where a collision
    /// would put two tenants on one publication or one replication slot.
    #[test]
    fn every_derivation_varies_with_the_app_id() {
        let first = AppId::mint();
        let second = AppId::mint();
        assert_ne!(first, second, "the control: two mints are two apps");

        assert_ne!(schema_name(&first), schema_name(&second));
        assert_ne!(
            role_name(&first).expect("composes"),
            role_name(&second).expect("composes")
        );
        assert_ne!(publication_name(&first), publication_name(&second));
        assert_ne!(
            worker_slot_name(&first, "worker-a").expect("composes"),
            worker_slot_name(&second, "worker-a").expect("composes")
        );
        assert_ne!(
            worker_slot_name_prefix(&first),
            worker_slot_name_prefix(&second)
        );
        assert_ne!(encryption_salt(&first), encryption_salt(&second));
        assert_ne!(attach_alias(&first), attach_alias(&second));
        assert_ne!(meter_key(&first), meter_key(&second));
        assert_ne!(kv_scope(&first), kv_scope(&second));
        assert_ne!(storage_prefix(&first), storage_prefix(&second));
        assert_ne!(lifecycle_lock_seed(&first), lifecycle_lock_seed(&second));
        assert_ne!(bundle_path_segment(&first), bundle_path_segment(&second));
        assert_ne!(ring_key(&first), ring_key(&second));
    }

    #[test]
    fn worker_slot_name_refuses_an_empty_worker_id() {
        assert_eq!(
            worker_slot_name(&fixture(), ""),
            Err(DerivationError::EmptyWorkerId)
        );
    }

    /// The role-name refusal is the pre-seam one, not a second opinion.
    ///
    /// An [`AppId`] cannot itself be long enough to trip this today, so the
    /// arm binds the two composers' agreement on where the limit is rather than
    /// constructing an over-long app id.
    #[test]
    fn the_role_name_refusal_is_the_pre_seam_refusal() {
        let long = "a".repeat(55);
        let direct = database_role::per_app_role_name(&long).expect_err("must refuse");
        assert_eq!(
            DerivationError::from(direct).to_string(),
            direct.to_string(),
            "the seam must not invent a second refusal message"
        );
    }

    /// The repository root, located from this crate rather than a working
    /// directory, so the arm below cannot be made vacuous by where it is run.
    fn repo_root() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("zeroship-core sits two levels below the repository root")
            .to_path_buf();
        assert!(
            root.join("AGENTS.md").is_file() && root.join("crates").is_dir(),
            "located {} as the repository root but it does not look like one",
            root.display()
        );
        root
    }

    /// Collect every shipped Rust source file: the `src` tree of every crate
    /// under `crates/` and `libs/`. Integration tests, benches and fixtures are
    /// out of scope by construction, which is deliberate - the control plane's
    /// workflow-engine test spells the pre-seam SQL on purpose, as an
    /// independent oracle for the lock key, and must keep doing so.
    fn production_sources(root: &Path) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let mut out = Vec::new();
        for workspace in ["crates", "libs"] {
            let Ok(members) = std::fs::read_dir(root.join(workspace)) else {
                continue;
            };
            for member in members.flatten() {
                walk(&member.path().join("src"), &mut out);
            }
        }
        assert!(
            out.iter().any(|p| p.ends_with("app_derivation.rs")),
            "the sweep did not reach this very file, so it is measuring nothing"
        );
        out
    }

    /// The app-lifecycle advisory-lock seed is spelled in one shipped file.
    ///
    /// The claim side takes this lock shared and the archive side takes it
    /// exclusive. They contend only if they hash the same text, and nothing
    /// fails when they do not - the archive marker just stops fencing claims.
    /// A fifth hand-spelled seed is therefore invisible at runtime, which is
    /// why it is caught here instead.
    ///
    /// MUTATION-CHECKED: pasting the literal back into any one of the four
    /// statements fails this arm and leaves every other test in the crate
    /// green.
    #[test]
    fn the_lifecycle_lock_seed_is_spelled_in_exactly_one_production_file() {
        let root = repo_root();
        let mut carriers: Vec<PathBuf> = production_sources(&root)
            .into_iter()
            .filter(|path| {
                std::fs::read_to_string(path)
                    .is_ok_and(|body| body.contains(LIFECYCLE_LOCK_SEED_PREFIX))
            })
            .collect();
        // Directory order is not defined; sort so a failure reads the same way
        // twice and the comparison below is against a set, not an ordering.
        carriers.sort();

        let expected = root.join("crates/zeroship-core/src/app_derivation.rs");
        assert_eq!(
            carriers,
            vec![expected],
            "the advisory-lock seed must be composed by `lifecycle_lock_seed` \
             and spelled nowhere else in shipped code"
        );
    }
}
