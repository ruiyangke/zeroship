include!("../../../tests/fixtures/platform_db/live_db.rs");

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal has to carry the three things a reader needs: WHICH
    /// database, WHAT was missing, and WHAT TO DO. Asserting on all three is
    /// the point -- a refusal that says only "not migrated" sends the reader
    /// back to guessing which of the eleven databases on :5440 it meant.
    ///
    /// THE APPLIER ASSERTION USED TO NAME A DELETED BINARY. It asked for
    /// `zeroship-platform-migrate`, which `ccda4bb42` removed on 2026-08-28 in
    /// the same change that rewrote `UNMIGRATED_REMEDY` to name
    /// `deploy/ops/db-migrate.sh`. The constant's own doc comment recorded the
    /// swap; the test did not, and stayed red from that day. It now asks for
    /// the wrapper the remedy actually prints, which is the string a reader
    /// would paste.
    #[test]
    fn a_refusal_names_the_database_the_gap_and_the_remedy() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let text = refusal(
            &where_,
            "it holds no schema \"zeroship\"",
            UNMIGRATED_REMEDY,
        );
        assert!(text.contains("127.0.0.1:5440/zeroship"), "{text}");
        assert!(text.contains("no schema \"zeroship\""), "{text}");
        assert!(text.contains("deploy/ops/db-migrate.sh"), "{text}");
        assert!(text.contains("NO TEST RAN"), "{text}");
    }

    /// A database whose journal is BEHIND the checkout refuses, and the refusal
    /// says which side is short and what to run.
    ///
    /// `ledger_verdict` is the whole second stage: everything above it is the
    /// one query that produces `applied`, and everything below it is printing.
    #[test]
    fn a_journal_behind_the_checkout_refuses_and_names_the_applier() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let carried = super::super::fingerprint::files_in(repo_root())
            .expect("this checkout carries a migration set")
            .len();
        let verdict = ledger_verdict(&where_, carried - 1);
        let text = verdict
            .refusal()
            .expect("one migration short of the tree is behind the tree");
        assert!(text.contains("BEHIND the tree"), "{text}");
        assert!(
            text.contains(&format!("of the {carried} migrations")),
            "{text}"
        );
        assert!(text.contains("deploy/ops/db-migrate.sh"), "{text}");
        assert!(text.contains("NO TEST RAN"), "{text}");
    }

    /// The control: a journal that has consumed the whole set is ready, and one
    /// that has consumed MORE is ready too.
    ///
    /// The second half is not padding. A database migrated by a checkout AHEAD
    /// of this one -- another agent's worktree, a branch merged since -- has
    /// more distinct checksums than this tree has files, and refusing it would
    /// turn every shared database into a permanent refusal for whoever is one
    /// commit behind. This stage answers "is the database BEHIND me", and that
    /// is a one-sided question on purpose.
    #[test]
    fn a_journal_level_with_or_ahead_of_the_checkout_is_ready() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let carried = super::super::fingerprint::files_in(repo_root())
            .expect("this checkout carries a migration set")
            .len();
        assert_eq!(ledger_verdict(&where_, carried), Verdict::Ready);
        assert_eq!(ledger_verdict(&where_, carried + 1), Verdict::Ready);
    }

    /// The ledger stage is off unless the caller said it needs the journal.
    ///
    /// A caller asking only for its own schema is not claiming to need the
    /// platform corpus, and counting a corpus it never wanted would refuse
    /// databases that are correct for it.
    #[test]
    fn the_ledger_stage_is_keyed_to_the_journal_schema() {
        let named = ["zeroship".to_string(), JOURNAL_SCHEMA.to_string()];
        let unnamed = ["zeroship".to_string()];
        assert!(ledger_stage_wanted(&named));
        assert!(!ledger_stage_wanted(&unnamed));
    }

    /// A refusal is pasted into issues and chat. The password must not travel
    /// with it.
    #[test]
    fn the_password_never_reaches_the_refusal() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let text = refusal(&where_, "unreachable", UNREACHABLE_REMEDY);
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("postgres:***@"), "{text}");
    }

    /// A DSN with no password must not print the USERNAME where the password
    /// would have been.
    #[test]
    fn a_password_less_dsn_prints_no_sentinel() {
        let where_ = Coordinates::of("postgres://postgres@127.0.0.1:5440/zeroship");
        assert_eq!(
            where_.redacted,
            "postgres://postgres@127.0.0.1:5440/zeroship"
        );
        assert_eq!(where_.database, "zeroship");
    }

    /// The three answers a reader can get must not share a remedy.
    ///
    /// This is the property GAP 1 broke without breaking any assertion: an
    /// unmigrated database was reported with the UNREACHABLE remedy, so two
    /// states that need different commands printed the same one. Distinctness
    /// is cheap to state and is what a caller relies on.
    #[test]
    fn the_states_a_reader_must_tell_apart_print_different_remedies() {
        let remedies = [
            UNREACHABLE_REMEDY,
            UNMIGRATED_REMEDY,
            ROLE_REMEDY,
            RUNTIME_REMEDY,
            DSN_REMEDY,
        ];
        for (index, one) in remedies.iter().enumerate() {
            for other in &remedies[index + 1..] {
                assert_ne!(one, other, "two states print the same remedy");
            }
        }
        // The one a database that only needs the corpus must get, and the one
        // it must NOT get.
        assert!(UNMIGRATED_REMEDY.contains("deploy/ops/db-migrate.sh"));
        assert!(!UNREACHABLE_REMEDY.contains("db-migrate.sh"));
        assert!(!ROLE_REMEDY.contains("db-migrate.sh"));
        // A kernel without io_uring is not fixed by editing a DSN.
        assert!(!RUNTIME_REMEDY.contains("provision_test_backends.sh"));
    }

    /// An unparseable DSN must refuse rather than reach the connect.
    #[test]
    fn an_unparseable_dsn_refuses_without_dialling() {
        let verdict = inspect("this is not a dsn", &["zeroship"]);
        let text = verdict.refusal().expect("a bare word is not a DSN");
        assert!(text.contains("REFUSED"), "{text}");
        assert!(text.contains("provision_test_backends.sh"), "{text}");
    }
}
