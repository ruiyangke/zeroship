include!("../../../tests/fixtures/platform_db/fingerprint.rs");

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir.join(MIGRATIONS_DIR)).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(MIGRATIONS_DIR).join(name), body).unwrap();
        }
    }

    /// A throwaway tree that looks like a repo root: only `db/migrations-ts`
    /// is ever read out of it. `TempDir` removes it on drop, so the caller
    /// holds it for the length of the test.
    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch tree")
    }

    #[test]
    fn identical_migration_sets_in_different_directories_agree() {
        // The absolute path differs between the two, which is the property that
        // has to hold or two worktrees of one commit never share a database.
        let (a, b) = (scratch(), scratch());
        tree(
            a.path(),
            &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")],
        );
        tree(
            b.path(),
            &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")],
        );
        let fa = of_dir(a.path()).unwrap();
        assert_eq!(fa, of_dir(b.path()).unwrap());
        assert_eq!(fa.len(), 12);
        assert!(fa.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_fingerprint_moves_when_the_schema_does() {
        // Three ways it can move, and all three matter: a function returning a
        // constant passes the agreement test above on its own.
        let base = scratch();
        tree(
            base.path(),
            &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")],
        );
        let f = of_dir(base.path()).unwrap();

        let added = scratch();
        tree(
            added.path(),
            &[
                ("20260101_one.ts", "one"),
                ("20260202_two.ts", "two"),
                ("20260303_three.ts", "three"),
            ],
        );
        assert_ne!(f, of_dir(added.path()).unwrap(), "an added migration");

        let edited = scratch();
        tree(
            edited.path(),
            &[
                ("20260101_one.ts", "one, but different"),
                ("20260202_two.ts", "two"),
            ],
        );
        assert_ne!(f, of_dir(edited.path()).unwrap(), "an edited migration");

        // Bytes untouched, name changed. The runner orders by filename and
        // journals under it, so this is a different schema; a digest over
        // contents alone would miss it.
        let renamed = scratch();
        tree(
            renamed.path(),
            &[("20260101_one.ts", "one"), ("20269999_two.ts", "two")],
        );
        assert_ne!(f, of_dir(renamed.path()).unwrap(), "a renamed migration");
    }

    #[test]
    fn an_empty_or_absent_migration_set_is_refused() {
        let empty = scratch();
        std::fs::create_dir_all(empty.path().join(MIGRATIONS_DIR)).unwrap();
        let err = of_dir(empty.path()).unwrap_err();
        assert!(err.contains("holds no *.ts migrations"), "{err}");

        let nodir = scratch();
        let err = of_dir(nodir.path()).unwrap_err();
        assert!(err.contains("no platform migrations directory"), "{err}");
    }

    /// `git add -A` then `git write-tree`, and hand back the tree's sha.
    ///
    /// A TREE rather than a commit on a branch, deliberately: `write-tree`
    /// needs no `user.email`, runs no hooks and signs nothing, so a case built
    /// on it cannot fail for a reason that has nothing to do with the
    /// fingerprint. [`of_ref`] takes any tree-ish, and the selftest's
    /// no-migrations case already passes it a bare tree sha.
    ///
    /// `add -A -f`: the force is against a GLOBAL excludes file. Nothing in a
    /// throwaway tree is ignorable, and a `*.ts` line in somebody's
    /// `~/.gitignore` would otherwise stage an empty set.
    fn write_tree(repo: &Path) -> String {
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["add", "-A", "-f"]);
        git(&["write-tree"]).trim().to_string()
    }

    fn init_repo(repo: &Path) {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["init", "-q", "."])
            .output()
            .expect("git init");
        assert!(out.status.success(), "git init: {out:?}");
    }

    #[test]
    fn the_git_tree_fingerprint_moves_when_one_migration_changes() {
        // THE NEGATIVE CONTROL for `of_ref`, and it is CONSTRUCTED rather than
        // sampled. The selftest used to take a commit 40 back on this branch
        // and assume its migrations differed; that binds the control to the
        // repository's recent activity, so it goes red during any quiet period
        // on migrations and its verdict says nothing about the function.
        //
        // Without a case of this shape, "the two computations agree on the real
        // repository" is satisfied by an `of_ref` that returns a constant --
        // and a constant fingerprint means every branch shares one database
        // with no symptom until two schemas collide in it.
        let repo = scratch();
        tree(
            repo.path(),
            &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")],
        );
        init_repo(repo.path());
        let before = write_tree(repo.path());

        std::fs::write(
            repo.path().join(MIGRATIONS_DIR).join("20260101_one.ts"),
            "one, but different",
        )
        .unwrap();
        let after = write_tree(repo.path());
        assert_ne!(before, after, "the two trees must actually differ");

        let fp_before = of_ref(repo.path(), &before).expect("a tree with migrations");
        let fp_after = of_ref(repo.path(), &after).expect("a tree with migrations");
        assert_ne!(
            fp_before, fp_after,
            "one edited migration must move the hash"
        );
    }

    #[test]
    fn the_two_computations_agree_on_a_constructed_tree() {
        // The real-repository pin in tests/lib_sweep_db_selftest.sh stays the
        // authority for this property -- a fixture cannot vouch for the files
        // the sweeper reasons over. What a fixture CAN do is put a name in the
        // set that the real repository does not have. The space matters: the
        // git side splits its listing on the TAB rather than on whitespace
        // precisely so a path with a space survives, and nothing tested it.
        let repo = scratch();
        tree(
            repo.path(),
            &[("20260101_one.ts", "one"), ("20260202 two.ts", "two")],
        );
        init_repo(repo.path());
        let from_ref = of_ref(repo.path(), &write_tree(repo.path())).expect("a tree");
        assert_eq!(of_dir(repo.path()).unwrap(), from_ref);
    }

    #[test]
    fn a_git_tree_fingerprint_ignores_everything_outside_the_migration_set() {
        // THE OTHER HALF of the control above, and the case that makes its
        // verdict attributable. Two trees that differ by one migration also
        // differ as OBJECTS, so "the hashes differ" is equally consistent with
        // a fingerprint keyed on the tree sha -- which would discriminate
        // beautifully and put every commit in its own database. Only a pair
        // where the tree moves and the migration set does not can tell those
        // apart, so this is that pair's second half.
        let repo = scratch();
        tree(repo.path(), &[("20260101_one.ts", "one")]);
        std::fs::write(repo.path().join("README.md"), "before").unwrap();
        init_repo(repo.path());
        let before = write_tree(repo.path());

        std::fs::write(repo.path().join("README.md"), "after").unwrap();
        let after = write_tree(repo.path());
        assert_ne!(before, after, "the trees must differ as objects");

        assert_eq!(
            of_ref(repo.path(), &before),
            of_ref(repo.path(), &after),
            "a file outside db/migrations-ts moved the fingerprint"
        );
    }

    #[test]
    fn a_git_tree_with_no_migrations_yields_no_value() {
        // Not a hash of nothing: that would be a legitimate-looking value no
        // working tree can produce, so every database keyed to it would look
        // reachable forever and the sweeper would never reclaim one.
        let repo = scratch();
        std::fs::write(repo.path().join("README.md"), "hello").unwrap();
        init_repo(repo.path());
        assert_eq!(of_ref(repo.path(), &write_tree(repo.path())), None);
    }

    #[test]
    fn a_non_ts_file_is_not_part_of_the_set() {
        // `discover_ts_files` takes `*.ts` only, so a README dropped in the
        // directory must not change which database the branch lands in.
        let (a, b) = (scratch(), scratch());
        tree(a.path(), &[("20260101_one.ts", "one")]);
        tree(
            b.path(),
            &[("20260101_one.ts", "one"), ("README.md", "hello")],
        );
        assert_eq!(of_dir(a.path()).unwrap(), of_dir(b.path()).unwrap());
    }
}
