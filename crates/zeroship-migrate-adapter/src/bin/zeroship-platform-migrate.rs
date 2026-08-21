//! `zeroship-platform-migrate` — Phase F Stage 4a.
//!
//! The platform-schema migrate driver, rebuilt on the PUBLISHED `zero-migrate`
//! engine. This REPLACES the retired in-tree `zeroship-migrate` CLI (the
//! docker-compose `migrate` one-shot service) as the producer of the platform
//! schema.
//!
//! Pipeline (per `db/migrations-ts/*.ts`, in filename order):
//!
//! ```text
//!   db/migrations-ts/NNNN_*.ts
//!        │  zeroship-runtime V8 isolate + STANDALONE v1 recorder (S2 mechanism)
//!        ▼
//!   { ir_version:1, name, ops }  envelope (authored in V8 — NOT hand-built)
//!        │  published zero-migrate: fail-closed load gate + guarded lower
//!        ▼  (Platform guard: cross-schema public/citext + roles/grants/functions)
//!   LoweredArtifact (plan steps + touched/created tables)
//!        │  MigrationEngine::apply_plan_with_touched_and_depends_scoped
//!        ▼  PostgresBackend::new_generic(&CompioPgSession)  — compio io_uring
//!   real platform DDL + append-only journal (zeroship_migrations)
//! ```
//!
//! Usage:
//!   zeroship-platform-migrate --database-url-file /path/to/dsn \
//!       [--migrations-dir db/migrations-ts] [--project-schema zeroship] \
//!       [--project-id zeroship] [--cluster-lock-database postgres]
//!
//! `--cluster-lock-database` names the database on the SAME cluster that
//! concurrent migrate runs coordinate through while applying cluster-global
//! objects (roles, databases, tablespaces). It defaults to `postgres` and only
//! needs setting on a cluster where that maintenance database was dropped. It
//! is a separate database from the one being migrated on purpose: a PostgreSQL
//! advisory lock is database-scoped, so two runs migrating two different
//! databases have nowhere else to exclude each other. See
//! `zeroship_migrate_adapter::platform::cluster_lock`.
//!
//! IN A DEPLOY FILE THE DSN ARRIVES AS A PATH, NEVER AS A VALUE. A process
//! argument list is public: `docker inspect`, `docker ps --no-trunc`, `ps` and
//! /proc/<pid>/cmdline all publish it to anything sharing the PID namespace,
//! and it survives in shell history and CI logs afterwards. A DSN admits
//! userinfo, so it is credential material by grammar - the same reason every
//! converted binary declares its DSN `Secret<String>` (crates/migrated/src/
//! config.rs) and gets `--<name>-file PATH` and nothing else from the macro.
//! This parser is hand-rolled and so was never covered by that guarantee, which
//! is how a postgres SUPERUSER DSN came to sit in the compose one-shot's argv;
//! `--database-url-file` is the projection the macro would have generated.
//!
//! `--database-url <dsn>` SURVIVES, and only because 29 in-repo test harnesses
//! pass a throwaway DSN that way against a scratch database on a dev box. It is
//! not a supported deploy shape and nothing under deploy/ may use it: check 6c
//! of tests/config_name_alignment_gate.sh fails any compose `command:` block
//! carrying a secret's value flag or a userinfo-bearing URL. Converting those
//! 29 callers to the path form is the change that lets this flag go; it was
//! left out of the fix deliberately, because 29 test files is not a reviewable
//! diff to bundle with a credential move.

use std::path::{Path, PathBuf};

use zeroship_migrate_adapter::platform::cluster_lock::DEFAULT_CLUSTER_LOCK_DATABASE;
use zeroship_migrate_adapter::platform::{run_platform_migrations, PlatformMigrateConfig};

fn main() {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("zeroship-platform-migrate: {msg}");
            eprintln!(
                "\nusage: zeroship-platform-migrate --database-url-file <PATH> \
                 [--migrations-dir <dir>] [--project-schema <schema>] [--project-id <id>] \\
                 [--cluster-lock-database <db>]"
            );
            std::process::exit(2);
        }
    };

    let result = compio::runtime::Runtime::new()
        .expect("build compio runtime")
        .block_on(run_platform_migrations(&cfg));

    match result {
        Ok(report) => {
            println!(
                "zeroship-platform-migrate: applied {} migration(s) across {} file(s) \
                 into schema '{}' (journal: {}_migrations)",
                report.applied.len(),
                report.files,
                cfg.project_schema,
                cfg.project_schema,
            );
            for name in &report.applied {
                println!("  applied  {name}");
            }
            for name in &report.skipped {
                println!("  skipped  {name} (already applied)");
            }
        }
        Err(e) => {
            eprintln!("zeroship-platform-migrate: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_args() -> Result<PlatformMigrateConfig, String> {
    parse_args_from(std::env::args().skip(1))
}

/// The argument loop, over any iterator so the rules below are testable.
fn parse_args_from<I>(args: I) -> Result<PlatformMigrateConfig, String>
where
    I: IntoIterator<Item = String>,
{
    let mut database_url_file: Option<String> = None;
    let mut database_url: Option<String> = None;
    let mut migrations_dir: Option<PathBuf> = None;
    let mut project_schema = String::from("zeroship");
    let mut project_id = String::from("zeroship");
    let mut cluster_lock_database = String::from(DEFAULT_CLUSTER_LOCK_DATABASE);

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database-url-file" => {
                database_url_file =
                    Some(args.next().ok_or("--database-url-file needs a value")?);
            }
            "--database-url" => {
                database_url = Some(args.next().ok_or("--database-url needs a value")?);
            }
            "--migrations-dir" => {
                migrations_dir =
                    Some(PathBuf::from(args.next().ok_or("--migrations-dir needs a value")?));
            }
            "--project-schema" => {
                project_schema = args.next().ok_or("--project-schema needs a value")?;
            }
            "--project-id" => {
                project_id = args.next().ok_or("--project-id needs a value")?;
            }
            "--cluster-lock-database" => {
                cluster_lock_database = args
                    .next()
                    .ok_or("--cluster-lock-database needs a value")?;
            }
            "-h" | "--help" => return Err("help".to_string()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }

    // Ambiguity is a refusal, not a precedence rule. Two DSNs mean the caller
    // does not know which database it is about to migrate, and silently
    // preferring one is how a migration lands somewhere nobody chose.
    let database_url = match (database_url_file, database_url) {
        (Some(_), Some(_)) => {
            return Err(
                "--database-url-file and --database-url are mutually exclusive; pass one"
                    .to_string(),
            )
        }
        // Same reader the generated `-file` flags use, so a DSN file written
        // by an editor and one written by `printf` are read identically here
        // and everywhere else, rather than by a second implementation.
        //
        // IT ENFORCES OWNER-ONLY PERMISSIONS, and it refuses rather than warns.
        // `read_secret_file` (crates/core/src/config/secrets.rs) calls
        // `enforce_owner_only`, which rejects any file with a bit set in 0o077.
        // MEASURED 2026-08-19 against the deployed image, one variable between
        // the arms: a 0644 DSN file gives
        //   secret file '...' has mode 100644; group and other permissions
        //   must be zero (chmod 600 '...')
        // and EXIT 2 -- the parse-failure code, not the migrate-failure code --
        // while the same file at 0600 parses and the run proceeds to connect.
        //
        // THIS COMMENT SAID THE OPPOSITE UNTIL 2026-08-19: "IT CHECKS NO
        // PERMISSIONS", citing a 2026-08-16 measurement of a 0644 file being
        // accepted. Whatever was run then, the code above it does check, so the
        // comment was an assertion of safety for a mode that is in fact
        // refused. Anyone provisioning a DSN file from this note would have
        // shipped an unbootable one-shot.
        //
        // What the path form buys on top of that is that the DSN is absent from
        // argv and from the environment.
        (Some(path), None) => zeroship_core::config::read_secret_file(&path)
            .map_err(|error| format!("--database-url-file {path}: {error}"))?,
        (None, Some(dsn)) => dsn,
        (None, None) => {
            return Err("a --database-url-file <PATH> (or --database-url <DSN>) is required".to_string())
        }
    };

    let migrations_dir = migrations_dir.unwrap_or_else(default_migrations_dir);

    Ok(PlatformMigrateConfig {
        database_url,
        migrations_dir,
        project_schema,
        project_id,
        cluster_lock_database,
    })
}

/// The default `db/migrations-ts` directory, resolved relative to the repo root
/// (the crate is two levels below it: `crates/zeroship-migrate-adapter`).
fn default_migrations_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .map(|root| root.join("db").join("migrations-ts"))
        .unwrap_or_else(|| PathBuf::from("db/migrations-ts"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::parse_args_from;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// A DSN file at 0600, matching what `zeroship dev init` writes. The mode
    /// is realism, not a precondition: the reader checks no permissions (see
    /// the note in `parse_args_from`), so these tests would pass at any mode.
    fn dsn_file(dir: &std::path::Path, contents: &str) -> String {
        let path = dir.join("migrate-dsn");
        let mut file = std::fs::File::create(&path).expect("create dsn file");
        file.write_all(contents.as_bytes()).expect("write dsn");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("tighten dsn file");
        path.to_string_lossy().into_owned()
    }

    /// THE CAPABILITY THE FIX NEEDED. Before this flag existed the ONLY way to
    /// reach this binary was `--database-url <dsn>`, so the compose one-shot
    /// had no choice but to put a postgres superuser password in its argv.
    ///
    /// Covers the parser. It does NOT cover deploy files continuing to spell
    /// the value form - the regression test for the defect itself is check 6c
    /// of tests/config_name_alignment_gate.sh, which reads deploy/compose and
    /// fails on a userinfo URL or a secret's value flag in a `command:` block.
    #[test]
    fn the_dsn_is_read_from_the_file_the_path_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dsn_file(dir.path(), "postgres://postgres:zeroship@postgres:5432/zeroship\n");
        let config = parse_args_from(argv(&["--database-url-file", &path]))
            .expect("a DSN supplied as a path must parse");
        assert_eq!(
            config.database_url,
            "postgres://postgres:zeroship@postgres:5432/zeroship",
            "the trailing newline must be trimmed by the shared secret reader"
        );
    }

    /// The one-variable partner for the test above: the SAME DSN passed the
    /// old way still parses, so that test measures the FILE path being read
    /// and not the parser rejecting everything.
    #[test]
    fn the_value_flag_still_parses_for_the_in_repo_harnesses() {
        let config = parse_args_from(argv(&[
            "--database-url",
            "postgres://postgres:zeroship@postgres:5432/zeroship",
        ]))
        .expect("the value form is still what 29 test harnesses pass");
        assert_eq!(
            config.database_url,
            "postgres://postgres:zeroship@postgres:5432/zeroship"
        );
    }

    /// Two DSNs is a refusal, not a precedence rule. A deploy file half-moved
    /// onto the path form would otherwise run against whichever one this
    /// parser happened to prefer.
    #[test]
    fn a_path_and_a_value_together_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dsn_file(dir.path(), "postgres://postgres:zeroship@postgres:5432/zeroship\n");
        let error = parse_args_from(argv(&[
            "--database-url-file",
            &path,
            "--database-url",
            "postgres://someone:else@elsewhere:5432/other",
        ]))
        .expect_err("two DSNs must refuse");
        assert!(
            error.contains("mutually exclusive"),
            "got {error:?}"
        );
    }

    /// No DSN at all is a refusal, not a compiled-in default. A default that
    /// works is how a deployment ends up pointed somewhere nobody chose.
    #[test]
    fn a_missing_dsn_is_refused() {
        let error =
            parse_args_from(argv(&["--project-id", "zeroship"])).expect_err("no DSN must refuse");
        assert!(
            error.contains("--database-url-file"),
            "the refusal must name what is missing; got {error:?}"
        );
    }

    /// A flag with no value must not silently take the next flag as its path.
    #[test]
    fn a_dangling_dsn_path_flag_is_refused() {
        let error = parse_args_from(argv(&["--database-url-file"]))
            .expect_err("a dangling flag must refuse");
        assert!(
            error.contains("needs a value"),
            "got {error:?}"
        );
    }
}
