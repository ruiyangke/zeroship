//! Routing keys and lock seeds derived from a canonical `AppId`.
//!
//! Services in separate processes derive these from the same app id, so each
//! is spelled once here. Golden vectors protect the derived values they share.

use crate::app_id::AppId;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The app-id fixture the rest of these tests derive from: the canonical
    /// rendering of the uuid `0191e7a2-b3c4-4d5e-8f90-123456789abc`.
    const FIXTURE: &str = "app_03cgepu94hyemwpcipafo7264";

    fn fixture() -> AppId {
        AppId::parse(FIXTURE).expect("the fixture is a canonical app id")
    }

    /// Every derived value, pinned to its bytes.
    ///
    /// Neither derivation has a `&str` composer to compare against, so the
    /// frozen literal is the only oracle for each.
    #[test]
    fn golden_vectors() {
        let app = fixture();

        // The text the advisory-lock statements bind.
        assert_eq!(
            lifecycle_lock_seed(&app),
            "zeroship:app-lifecycle:app_03cgepu94hyemwpcipafo7264"
        );

        // The ring hashes the printed id. Nothing stores a ring position, so a
        // re-key costs a cold start and leaves no stale state behind.
        assert_eq!(ring_key(&app), FIXTURE.as_bytes());
    }

    /// Every derivation is a function OF THE ID, not of a constant.
    ///
    /// The golden vectors above drive one app, so a composer that ignored its
    /// argument and returned its own frozen literal would pass all of them.
    /// This is the arm that refuses that: two distinct ids must disagree
    /// everywhere.
    #[test]
    fn every_derivation_varies_with_the_app_id() {
        let first = AppId::mint();
        let second = AppId::mint();
        assert_ne!(first, second, "the control: two mints are two apps");

        assert_ne!(lifecycle_lock_seed(&first), lifecycle_lock_seed(&second));
        assert_ne!(ring_key(&first), ring_key(&second));
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
