//! Phase-4 port regression gate (design §10): faithfully apply the WHOLE ported
//! `db/migrations/` set (the 56 `V<NNNN>__` files produced from the Liquibase
//! changelog) on a fresh DB under the PLATFORM profile, then assert the resulting
//! schema is the one Liquibase used to build — namespaces, roles, RLS policies,
//! trigger functions, and key tables.
//!
//! This drives the REAL `command::runner::run_migrate` (not a shim, not a
//! spawned process) so the port is guarded by the same engine + guard the compose
//! `migrate` service will run. The privileged DDL in these files (CREATE ROLE /
//! GRANT / CREATE SCHEMA / ENABLE RLS / CREATE POLICY) is EXACTLY why the run must
//! be Platform: under Confined every one of those is DENIED.
//!
//! The ported tree uses the hardcoded `zeroship` + `oauth_hydra` schemas and the
//! global `zeroship_*` / `oauth_hydra` roles, so this test cannot use a
//! token-suffixed throwaway schema like `cli_platform_pg.rs`. It instead resets
//! the two real schemas in the dedicated `zeroship_migrate_test` DB and uses a
//! token-suffixed META (journal) schema so concurrent runs do not collide on the
//! journal. Roles are cluster-wide; the files' `IF NOT EXISTS` / DO-block guards
//! make their creation idempotent across runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use compio_postgres::Client;
use zeroship_migrate::command::runner::{
    run_migrate, run_rollback, run_status, RunConfig, RunProfile, RunReport,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";
const DEFAULT_LIQUIBASE_CHANGELOG_DIR: &str = "/home/ruiyang/Projects/appbase/db/changelog";
const DEFAULT_LIQUIBASE_IMAGE: &str = "liquibase/liquibase:4.31";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    pg_for_url(&dsn()).await
}

async fn pg_for_url(database_url: &str) -> Client {
    let (client, conn) = compio_postgres::connect(database_url, compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn short_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{:x}_{n}", nanos & 0xffff_ffff_ffff)
}

fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

/// The repo-root `db/migrations/` directory (the ported set under test).
fn migrations_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/zeroship-migrate
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../db/migrations")
        .canonicalize()
        .expect("db/migrations exists at repo root")
}

fn liquibase_changelog_dir() -> PathBuf {
    std::env::var("MIGRATE_LIQUIBASE_CHANGELOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_LIQUIBASE_CHANGELOG_DIR))
        .canonicalize()
        .expect("original Liquibase db/changelog directory exists")
}

fn liquibase_image() -> String {
    std::env::var("MIGRATE_LIQUIBASE_IMAGE")
        .unwrap_or_else(|_| DEFAULT_LIQUIBASE_IMAGE.to_string())
}

fn dsn_field(name: &str, default: &str) -> String {
    let prefix = format!("{name}=");
    dsn()
        .split_ascii_whitespace()
        .find_map(|part| part.strip_prefix(&prefix).map(str::to_string))
        .unwrap_or_else(|| default.to_string())
}

fn dsn_with_db(dbname: &str) -> String {
    let mut found = false;
    let mut parts = Vec::new();
    for part in dsn().split_ascii_whitespace() {
        if part.starts_with("dbname=") {
            parts.push(format!("dbname={dbname}"));
            found = true;
        } else {
            parts.push(part.to_string());
        }
    }
    if !found {
        parts.push(format!("dbname={dbname}"));
    }
    parts.join(" ")
}

fn liquibase_jdbc_url(dbname: &str) -> String {
    let host = dsn_field("host", "localhost");
    let port = dsn_field("port", "5440");
    format!("jdbc:postgresql://{host}:{port}/{dbname}")
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A Platform [`RunConfig`] over the REAL `zeroship` schema with the
/// `oauth_hydra` + `public` namespaces in the allowlist (so 0027's `CREATE SCHEMA
/// oauth_hydra` is in-allowlist), and a UNIQUE meta (journal) schema so the journal
/// does not collide with a concurrent run.
fn platform_cfg(meta: &str, yes: bool) -> RunConfig {
    platform_cfg_for_url(dsn(), meta, yes)
}

fn platform_cfg_for_url(database_url: String, meta: &str, yes: bool) -> RunConfig {
    RunConfig {
        dir: migrations_dir(),
        database_url,
        engine_override: None,
        profile: RunProfile::Platform,
        project_id: "platform".to_string(),
        project_schema: "zeroship".to_string(),
        schemas: vec![
            "zeroship".to_string(),
            "oauth_hydra".to_string(),
            "public".to_string(),
        ],
        // The changelog installs citext (V0001) + uuid-ossp (V0027); the guard
        // gates `CREATE EXTENSION` against this allowlist, so both must be named.
        extensions: vec!["citext".to_string(), "uuid-ossp".to_string()],
        meta_schema: meta.to_string(),
        yes,
        statement_timeout: std::time::Duration::from_secs(120),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

/// Reset the platform schemas + journal so the run starts from a known-fresh DB.
/// Roles are cluster-wide and left intact (the files' guards make re-create a
/// no-op); we only reset the per-DB schema state and the journal.
async fn reset(conn: &Client, meta: &str) {
    conn.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS zeroship CASCADE; \
         DROP SCHEMA IF EXISTS oauth_hydra CASCADE; \
         DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"
    ))
    .await
    .expect("reset platform schemas + journal");
}

async fn recreate_database(admin: &Client, dbname: &str) {
    admin
        .execute(
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quote_ident(dbname)),
            &[],
        )
        .await
        .unwrap_or_else(|e| panic!("drop throwaway database {dbname}: {e:?}"));
    admin
        .execute(&format!("CREATE DATABASE {}", quote_ident(dbname)), &[])
        .await
        .unwrap_or_else(|e| panic!("create throwaway database {dbname}: {e:?}"));
}

async fn drop_database(admin: &Client, dbname: &str) {
    let _ = admin
        .execute(
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quote_ident(dbname)),
            &[],
        )
        .await;
}

fn run_liquibase_update(dbname: &str, changelog_dir: &Path) {
    let image = liquibase_image();
    let mount = format!("{}:/liquibase/changelog:ro", changelog_dir.display());
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "host",
            "-v",
            &mount,
            &image,
            &format!("--url={}", liquibase_jdbc_url(dbname)),
            &format!("--username={}", dsn_field("user", "postgres")),
            &format!("--password={}", dsn_field("password", "zeroship")),
            "--changelog-file=changelog/db.changelog-master.yaml",
            "--liquibase-schema-name=public",
            "update",
        ])
        .output()
        .expect("spawn dockerized Liquibase");

    assert!(
        output.status.success(),
        "Liquibase update failed for {dbname}\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn collect_lines(conn: &Client, label: &str, sql: &str) -> Vec<String> {
    let rows = conn
        .query(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("collect {label} fingerprint: {e:?}\nSQL:\n{sql}"));
    rows.iter()
        .map(|r| canonicalize_catalog_line(label, r.get::<_, String>(0)))
        .collect()
}

fn canonicalize_catalog_line(label: &str, line: String) -> String {
    if label == "functions" {
        return canonicalize_function_body_blank_lines(&line);
    }
    line
}

fn canonicalize_function_body_blank_lines(line: &str) -> String {
    fn strip_ws_only_segments(line: &str, sep: &str) -> String {
        line.split(sep)
            .map(|part| {
                if part.chars().all(|c| c == ' ' || c == '\t') {
                    ""
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join(sep)
    }

    let line = strip_ws_only_segments(line, "\\n");
    strip_ws_only_segments(&line, "\n")
}

fn diff_lines(left: &[String], right: &[String]) -> String {
    let l: BTreeSet<_> = left.iter().cloned().collect();
    let r: BTreeSet<_> = right.iter().cloned().collect();
    let only_left = l.difference(&r).take(80).cloned().collect::<Vec<_>>();
    let only_right = r.difference(&l).take(80).cloned().collect::<Vec<_>>();
    format!(
        "left_count={} right_count={} only_left(first {})=\n{}\nonly_right(first {})=\n{}",
        left.len(),
        right.len(),
        only_left.len(),
        only_left.join("\n"),
        only_right.len(),
        only_right.join("\n")
    )
}

/// Full structural platform fingerprint used by the Liquibase-vs-op diff.
///
/// Object coverage:
/// schemas, the fixed platform roles + role settings, relations, columns,
/// primary/foreign/unique/check/exclusion/domain constraints, indexes, sequences,
/// views, triggers, functions, enum/domain/type metadata, RLS flags, policies,
/// extensions, expanded grants, default ACLs, and comments.
///
/// Explicit allowlist: Liquibase's own `public.databasechangelog` and
/// `public.databasechangeloglock` bookkeeping relations (including their row
/// types), plus the port runner's public migration-journal immutability function.
/// They are migration metadata, not platform schema. Everything else in
/// `zeroship`, `oauth_hydra`, and `public` is compared byte-for-byte after both
/// sides are introspected with the same pg_catalog/information_schema queries.
/// The only catalog canonicalization is inside stored function bodies: Liquibase
/// strips line comments in dollar-quoted PL/pgSQL and leaves whitespace-only body
/// lines. The port keeps true blank lines to avoid committing trailing
/// whitespace, so whitespace-only function lines are compared as blank lines;
/// all nonblank function text is still byte-compared.
async fn platform_catalog_fingerprint(conn: &Client) -> Vec<String> {
    let queries: &[(&str, &str)] = &[
        (
            "schemas",
            r#"
            SELECT concat(
                'schema|', n.nspname,
                '|owner=', pg_get_userbyid(n.nspowner),
                '|acl=', COALESCE(n.nspacl::text, ''),
                '|comment=', COALESCE(obj_description(n.oid, 'pg_namespace'), '')
            ) AS line
            FROM pg_namespace n
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
        (
            "roles",
            r#"
            SELECT concat(
                'role|', r.rolname,
                '|login=', r.rolcanlogin,
                '|super=', r.rolsuper,
                '|inherit=', r.rolinherit,
                '|createdb=', r.rolcreatedb,
                '|createrole=', r.rolcreaterole,
                '|replication=', r.rolreplication,
                '|bypassrls=', r.rolbypassrls,
                '|connlimit=', r.rolconnlimit,
                '|validuntil=', COALESCE(r.rolvaliduntil::text, ''),
                '|config=', COALESCE((
                    SELECT string_agg(sv, ',' ORDER BY sv)
                    FROM pg_db_role_setting s
                    CROSS JOIN LATERAL unnest(s.setconfig) AS cfg(sv)
                    WHERE s.setrole = r.oid AND s.setdatabase = 0
                ), '')
            ) AS line
            FROM pg_roles r
            WHERE r.rolname IN (
                'zeroship_auth',
                'zeroship_control',
                'zeroship_gateway',
                'zeroship_worker',
                'zeroship_app',
                'oauth_hydra'
            )
            ORDER BY 1
            "#,
        ),
        (
            "relations",
            r#"
            SELECT concat(
                'relation|', n.nspname, '.', c.relname,
                '|kind=', c.relkind,
                '|owner=', pg_get_userbyid(c.relowner),
                '|persistence=', c.relpersistence,
                '|am=', COALESCE(am.amname, ''),
                '|tablespace=', COALESCE(ts.spcname, ''),
                '|options=', COALESCE(array_to_string(c.reloptions, ','), ''),
                '|rls=', c.relrowsecurity,
                '|forcerls=', c.relforcerowsecurity,
                '|comment=', COALESCE(obj_description(c.oid, 'pg_class'), '')
            ) AS line
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_am am ON am.oid = c.relam
            LEFT JOIN pg_tablespace ts ON ts.oid = c.reltablespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
              AND NOT (n.nspname = 'public' AND c.relname IN ('databasechangelog', 'databasechangeloglock'))
            ORDER BY 1
            "#,
        ),
        (
            "columns",
            r#"
            SELECT concat(
                'column|', n.nspname, '.', c.relname, '.', a.attname,
                '|num=', a.attnum,
                '|type=', format_type(a.atttypid, a.atttypmod),
                '|notnull=', a.attnotnull,
                '|identity=', a.attidentity,
                '|generated=', a.attgenerated,
                '|default=', replace(replace(replace(COALESCE(pg_get_expr(ad.adbin, ad.adrelid), ''), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|collation=', COALESCE(coll.collname, ''),
                '|storage=', a.attstorage,
                '|compression=', a.attcompression,
                '|comment=', COALESCE(col_description(c.oid, a.attnum), '')
            ) AS line
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
            LEFT JOIN pg_collation coll ON coll.oid = a.attcollation AND a.attcollation <> 0
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
              AND a.attnum > 0
              AND NOT a.attisdropped
              AND NOT (n.nspname = 'public' AND c.relname IN ('databasechangelog', 'databasechangeloglock'))
            ORDER BY 1
            "#,
        ),
        (
            "constraints",
            r#"
            SELECT concat(
                'constraint|', n.nspname, '.', c.relname, '.', con.conname,
                '|type=', con.contype,
                '|deferrable=', con.condeferrable,
                '|deferred=', con.condeferred,
                '|validated=', con.convalidated,
                '|match=', con.confmatchtype,
                '|update=', con.confupdtype,
                '|delete=', con.confdeltype,
                '|cols=', COALESCE((
                    SELECT string_agg(a.attname, ',' ORDER BY k.ord)
                    FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                    JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
                ), ''),
                '|ref=', COALESCE(rn.nspname || '.' || rc.relname, ''),
                '|refcols=', COALESCE((
                    SELECT string_agg(a.attname, ',' ORDER BY k.ord)
                    FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                    JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum
                ), ''),
                '|def=', replace(replace(replace(pg_get_constraintdef(con.oid, true), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|comment=', COALESCE(obj_description(con.oid, 'pg_constraint'), '')
            ) AS line
            FROM pg_constraint con
            JOIN pg_class c ON c.oid = con.conrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_class rc ON rc.oid = con.confrelid
            LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND NOT (n.nspname = 'public' AND c.relname IN ('databasechangelog', 'databasechangeloglock'))
            ORDER BY 1
            "#,
        ),
        (
            "domain_constraints",
            r#"
            SELECT concat(
                'domain_constraint|', n.nspname, '.', t.typname, '.', con.conname,
                '|validated=', con.convalidated,
                '|def=', replace(replace(replace(pg_get_constraintdef(con.oid, true), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|comment=', COALESCE(obj_description(con.oid, 'pg_constraint'), '')
            ) AS line
            FROM pg_constraint con
            JOIN pg_type t ON t.oid = con.contypid
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
        (
            "indexes",
            r#"
            SELECT concat(
                'index|', tn.nspname, '.', tc.relname, '.', ic.relname,
                '|unique=', i.indisunique,
                '|primary=', i.indisprimary,
                '|exclusion=', i.indisexclusion,
                '|valid=', i.indisvalid,
                '|ready=', i.indisready,
                '|def=', replace(replace(replace(pg_get_indexdef(ic.oid), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|predicate=', replace(replace(replace(COALESCE(pg_get_expr(i.indpred, i.indrelid), ''), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|expressions=', replace(replace(replace(COALESCE(pg_get_expr(i.indexprs, i.indrelid), ''), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|comment=', COALESCE(obj_description(ic.oid, 'pg_class'), '')
            ) AS line
            FROM pg_index i
            JOIN pg_class ic ON ic.oid = i.indexrelid
            JOIN pg_class tc ON tc.oid = i.indrelid
            JOIN pg_namespace tn ON tn.oid = tc.relnamespace
            WHERE tn.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND NOT (tn.nspname = 'public' AND tc.relname IN ('databasechangelog', 'databasechangeloglock'))
            ORDER BY 1
            "#,
        ),
        (
            "sequences",
            r#"
            SELECT concat(
                'sequence|', n.nspname, '.', c.relname,
                '|type=', format_type(s.seqtypid, NULL::integer),
                '|start=', s.seqstart,
                '|increment=', s.seqincrement,
                '|min=', s.seqmin,
                '|max=', s.seqmax,
                '|cache=', s.seqcache,
                '|cycle=', s.seqcycle,
                '|owned_by=', COALESCE(onsp.nspname || '.' || oc.relname || '.' || oa.attname, '')
            ) AS line
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_sequence s ON s.seqrelid = c.oid
            LEFT JOIN pg_depend d ON d.objid = c.oid AND d.classid = 'pg_class'::regclass AND d.deptype IN ('a', 'i')
            LEFT JOIN pg_class oc ON oc.oid = d.refobjid
            LEFT JOIN pg_namespace onsp ON onsp.oid = oc.relnamespace
            LEFT JOIN pg_attribute oa ON oa.attrelid = d.refobjid AND oa.attnum = d.refobjsubid
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
        (
            "views",
            r#"
            SELECT concat(
                'view|', n.nspname, '.', c.relname,
                '|kind=', c.relkind,
                '|def=', replace(replace(replace(pg_get_viewdef(c.oid, true), E'\n', '\n'), E'\r', '\r'), E'\t', '\t')
            ) AS line
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND c.relkind IN ('v', 'm')
            ORDER BY 1
            "#,
        ),
        (
            "triggers",
            r#"
            SELECT concat(
                'trigger|', n.nspname, '.', c.relname, '.', t.tgname,
                '|enabled=', t.tgenabled,
                '|def=', replace(replace(replace(pg_get_triggerdef(t.oid, true), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|comment=', COALESCE(obj_description(t.oid, 'pg_trigger'), '')
            ) AS line
            FROM pg_trigger t
            JOIN pg_class c ON c.oid = t.tgrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND NOT t.tgisinternal
              AND NOT (n.nspname = 'public' AND c.relname IN ('databasechangelog', 'databasechangeloglock'))
            ORDER BY 1
            "#,
        ),
        (
            "functions",
            r#"
            SELECT concat(
                'function|', n.nspname, '.', p.proname, '(', pg_get_function_identity_arguments(p.oid), ')',
                '|kind=', p.prokind,
                '|returns=', pg_get_function_result(p.oid),
                '|lang=', l.lanname,
                '|volatility=', p.provolatile,
                '|secdef=', p.prosecdef,
                '|strict=', p.proisstrict,
                '|leakproof=', p.proleakproof,
                '|parallel=', p.proparallel,
                '|config=', COALESCE(array_to_string(p.proconfig, ','), ''),
                '|src=', replace(replace(replace(COALESCE(p.prosrc, ''), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|def=', replace(replace(replace(
                    CASE WHEN p.prokind = 'a' THEN '' ELSE COALESCE(pg_get_functiondef(p.oid), '') END,
                    E'\n',
                    '\n'
                ), E'\r', '\r'), E'\t', '\t'),
                '|comment=', COALESCE(obj_description(p.oid, 'pg_proc'), '')
            ) AS line
            FROM pg_proc p
            JOIN pg_namespace n ON n.oid = p.pronamespace
            JOIN pg_language l ON l.oid = p.prolang
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND NOT (n.nspname = 'public' AND p.proname LIKE '%schema_migrations_immutable')
            ORDER BY 1
            "#,
        ),
        (
            "types",
            r#"
            SELECT concat(
                'type|', n.nspname, '.', t.typname,
                '|kind=', t.typtype,
                '|category=', t.typcategory,
                '|owner=', pg_get_userbyid(t.typowner),
                '|base=', CASE WHEN t.typbasetype = 0 THEN '' ELSE format_type(t.typbasetype, t.typtypmod) END,
                '|notnull=', t.typnotnull,
                '|default=', COALESCE(t.typdefault, ''),
                '|collation=', COALESCE(coll.collname, ''),
                '|input=', t.typinput::regproc::text,
                '|output=', t.typoutput::regproc::text,
                '|comment=', COALESCE(obj_description(t.oid, 'pg_type'), '')
            ) AS line
            FROM pg_type t
            JOIN pg_namespace n ON n.oid = t.typnamespace
            LEFT JOIN pg_collation coll ON coll.oid = t.typcollation AND t.typcollation <> 0
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND t.typisdefined
              AND NOT (
                  n.nspname = 'public'
                  AND t.typname IN (
                      'databasechangelog',
                      'databasechangeloglock',
                      '_databasechangelog',
                      '_databasechangeloglock'
                  )
              )
            ORDER BY 1
            "#,
        ),
        (
            "enums",
            r#"
            SELECT concat(
                'enum|', n.nspname, '.', t.typname,
                '|sort=', e.enumsortorder,
                '|label=', e.enumlabel
            ) AS line
            FROM pg_enum e
            JOIN pg_type t ON t.oid = e.enumtypid
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
        (
            "policies",
            r#"
            SELECT concat(
                'policy|', n.nspname, '.', c.relname, '.', p.polname,
                '|cmd=', p.polcmd,
                '|permissive=', p.polpermissive,
                '|roles=', COALESCE((
                    SELECT string_agg(CASE WHEN r.oid = 0 THEN 'public' ELSE pr.rolname END, ',' ORDER BY CASE WHEN r.oid = 0 THEN 'public' ELSE pr.rolname END)
                    FROM unnest(p.polroles) AS r(oid)
                    LEFT JOIN pg_roles pr ON pr.oid = r.oid
                ), ''),
                '|using=', replace(replace(replace(COALESCE(pg_get_expr(p.polqual, p.polrelid), ''), E'\n', '\n'), E'\r', '\r'), E'\t', '\t'),
                '|check=', replace(replace(replace(COALESCE(pg_get_expr(p.polwithcheck, p.polrelid), ''), E'\n', '\n'), E'\r', '\r'), E'\t', '\t')
            ) AS line
            FROM pg_policy p
            JOIN pg_class c ON c.oid = p.polrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
        (
            "extensions",
            r#"
            SELECT concat(
                'extension|', e.extname,
                '|schema=', n.nspname,
                '|version=', e.extversion
            ) AS line
            FROM pg_extension e
            JOIN pg_namespace n ON n.oid = e.extnamespace
            ORDER BY 1
            "#,
        ),
        (
            "schema_grants",
            r#"
            SELECT concat(
                'grant|schema|', n.nspname,
                '|grantee=', CASE WHEN x.grantee = 0 THEN 'public' ELSE grantee.rolname END,
                '|grantor=', grantor.rolname,
                '|priv=', x.privilege_type,
                '|grantable=', x.is_grantable
            ) AS line
            FROM pg_namespace n
            CROSS JOIN LATERAL aclexplode(COALESCE(n.nspacl, acldefault('n', n.nspowner))) AS x
            LEFT JOIN pg_roles grantee ON grantee.oid = x.grantee
            JOIN pg_roles grantor ON grantor.oid = x.grantor
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
        (
            "relation_grants",
            r#"
            SELECT concat(
                'grant|relation|', n.nspname, '.', c.relname,
                '|grantee=', CASE WHEN x.grantee = 0 THEN 'public' ELSE grantee.rolname END,
                '|grantor=', grantor.rolname,
                '|priv=', x.privilege_type,
                '|grantable=', x.is_grantable
            ) AS line
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault(CASE WHEN c.relkind = 'S' THEN 'S'::"char" ELSE 'r'::"char" END, c.relowner))) AS x
            LEFT JOIN pg_roles grantee ON grantee.oid = x.grantee
            JOIN pg_roles grantor ON grantor.oid = x.grantor
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
              AND NOT (n.nspname = 'public' AND c.relname IN ('databasechangelog', 'databasechangeloglock'))
            ORDER BY 1
            "#,
        ),
        (
            "function_grants",
            r#"
            SELECT concat(
                'grant|function|', n.nspname, '.', p.proname, '(', pg_get_function_identity_arguments(p.oid), ')',
                '|grantee=', CASE WHEN x.grantee = 0 THEN 'public' ELSE grantee.rolname END,
                '|grantor=', grantor.rolname,
                '|priv=', x.privilege_type,
                '|grantable=', x.is_grantable
            ) AS line
            FROM pg_proc p
            JOIN pg_namespace n ON n.oid = p.pronamespace
            CROSS JOIN LATERAL aclexplode(COALESCE(p.proacl, acldefault('f', p.proowner))) AS x
            LEFT JOIN pg_roles grantee ON grantee.oid = x.grantee
            JOIN pg_roles grantor ON grantor.oid = x.grantor
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND NOT (n.nspname = 'public' AND p.proname LIKE '%schema_migrations_immutable')
            ORDER BY 1
            "#,
        ),
        (
            "type_grants",
            r#"
            SELECT concat(
                'grant|type|', n.nspname, '.', t.typname,
                '|grantee=', CASE WHEN x.grantee = 0 THEN 'public' ELSE grantee.rolname END,
                '|grantor=', grantor.rolname,
                '|priv=', x.privilege_type,
                '|grantable=', x.is_grantable
            ) AS line
            FROM pg_type t
            JOIN pg_namespace n ON n.oid = t.typnamespace
            CROSS JOIN LATERAL aclexplode(COALESCE(t.typacl, acldefault('T', t.typowner))) AS x
            LEFT JOIN pg_roles grantee ON grantee.oid = x.grantee
            JOIN pg_roles grantor ON grantor.oid = x.grantor
            WHERE n.nspname IN ('zeroship', 'oauth_hydra', 'public')
              AND t.typisdefined
              AND NOT (
                  n.nspname = 'public'
                  AND t.typname IN (
                      'databasechangelog',
                      'databasechangeloglock',
                      '_databasechangelog',
                      '_databasechangeloglock'
                  )
              )
            ORDER BY 1
            "#,
        ),
        (
            "default_acls",
            r#"
            SELECT concat(
                'default_acl|owner=', pg_get_userbyid(d.defaclrole),
                '|schema=', COALESCE(n.nspname, ''),
                '|type=', d.defaclobjtype,
                '|acl=', COALESCE(d.defaclacl::text, '')
            ) AS line
            FROM pg_default_acl d
            LEFT JOIN pg_namespace n ON n.oid = d.defaclnamespace
            WHERE n.nspname IS NULL OR n.nspname IN ('zeroship', 'oauth_hydra', 'public')
            ORDER BY 1
            "#,
        ),
    ];

    let mut out = Vec::new();
    for (label, sql) in queries {
        out.extend(collect_lines(conn, label, sql).await);
    }
    out.sort();
    out
}

async fn namespace_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_namespace WHERE nspname = $1", &[&name])
        .await
        .expect("query pg_namespace")
        .is_empty()
}

async fn role_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name])
        .await
        .expect("query pg_roles")
        .is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence")
        .is_empty()
}

async fn policy_exists(conn: &Client, schema: &str, table: &str, policy: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_policy p \
               JOIN pg_class c ON c.oid = p.polrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND p.polname = $3",
            &[&schema, &table, &policy],
        )
        .await
        .expect("query pg_policy")
        .is_empty()
}

// ---------------------------------------------------------------------------
// The port gate: the full ported set applies under Platform, the expected
// schema inventory materializes, and a re-run is an idempotent no-op.
// ---------------------------------------------------------------------------

#[compio::test]
async fn ported_changelog_applies_under_platform_and_materializes_the_schema() {
    let conn = pg().await;
    let meta = format!("portmeta_{}", token());
    reset(&conn, &meta).await;

    let cfg = platform_cfg(&meta, /* yes */ true);

    // 1. The WHOLE ported set applies with no error under Platform.
    let report = run_migrate(&cfg)
        .await
        .expect("the ported db/migrations set applies cleanly under Platform");
    let applied = match report {
        RunReport::Migrate(outcome) => {
            assert!(!outcome.is_noop(), "a fresh DB applies migrations");
            outcome.applied.len()
        }
        other => panic!("expected Migrate report, got {other:?}"),
    };
    assert_eq!(applied, 56, "all 56 ported files applied (0045 is a gap)");

    // 2. Namespaces (the two the changelog provisions).
    assert!(namespace_exists(&conn, "zeroship").await, "zeroship schema");
    assert!(namespace_exists(&conn, "oauth_hydra").await, "oauth_hydra schema");

    // 3. Roles — the five platform service roles + the Hydra role (0025 / 0027).
    for role in [
        "zeroship_auth",
        "zeroship_control",
        "zeroship_gateway",
        "zeroship_worker",
        "zeroship_app",
        "oauth_hydra",
    ] {
        assert!(role_exists(&conn, role).await, "role {role} created");
    }

    // 4. Key tables across the auth / control / sandbox / billing domains.
    for (schema, table) in [
        ("zeroship", "users"),
        ("zeroship", "apps"),
        ("zeroship", "app_secrets"),
        ("zeroship", "gateway_sessions"),
        ("zeroship", "app_session_anchors"),
        ("zeroship", "app_user_identities"),
        ("zeroship", "app_members"),
        ("zeroship", "oauth_clients"),
        ("zeroship", "audit_events"),
        ("zeroship", "rate_limits"),
        ("zeroship", "sandboxes"),
        ("zeroship", "plans"),
        ("zeroship", "invoices"),
    ] {
        assert!(
            table_exists(&conn, schema, table).await,
            "{schema}.{table} exists"
        );
    }

    // 5. RLS policies — the four tenant_isolation policies 0025 installs.
    for table in [
        "app_secrets",
        "gateway_sessions",
        "app_session_anchors",
        "app_user_identities",
    ] {
        assert!(
            policy_exists(&conn, "zeroship", table, "tenant_isolation").await,
            "RLS policy tenant_isolation on zeroship.{table}"
        );
    }

    // 6. Idempotent re-run: nothing pending, no-op.
    let report2 = run_migrate(&cfg).await.expect("idempotent re-run");
    match report2 {
        RunReport::Migrate(outcome) => assert!(outcome.is_noop(), "re-run is a no-op"),
        other => panic!("expected Migrate report, got {other:?}"),
    }

    // 7. Status: all 56 applied, none pending.
    match run_status(&cfg).await.expect("status reads journal") {
        RunReport::Status(status) => {
            assert_eq!(status.applied.len(), 56, "56 applied");
            assert!(status.pending.is_empty(), "nothing pending");
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    // Clean up the journal (leave the schemas; the next run resets them).
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"))
        .await
        .expect("drop journal schema");
}

#[compio::test]
async fn ported_changelog_is_full_schema_equivalent_to_original_liquibase() {
    let tok = short_token();
    let liquibase_db = format!("zsdiff_liq_{tok}");
    let port_db = format!("zsdiff_port_{tok}");
    let meta = format!("zsdiff_meta_{tok}");
    let changelog_dir = liquibase_changelog_dir();

    let admin = pg_for_url(&dsn_with_db("postgres")).await;
    recreate_database(&admin, &liquibase_db).await;
    recreate_database(&admin, &port_db).await;

    // A: the original production Liquibase changelog.
    run_liquibase_update(&liquibase_db, &changelog_dir);
    let liquibase_conn = pg_for_url(&dsn_with_db(&liquibase_db)).await;
    let liquibase_fp = platform_catalog_fingerprint(&liquibase_conn).await;

    // B: the ported op/Flyway directory through the real Platform runner.
    let port_cfg = platform_cfg_for_url(dsn_with_db(&port_db), &meta, /* yes */ true);
    match run_migrate(&port_cfg)
        .await
        .expect("the ported db/migrations set applies cleanly under Platform")
    {
        RunReport::Migrate(outcome) => {
            assert_eq!(
                outcome.applied.len(),
                56,
                "all 56 ported files applied in the diff-to-zero database"
            );
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    let port_conn = pg_for_url(&dsn_with_db(&port_db)).await;
    let port_fp = platform_catalog_fingerprint(&port_conn).await;

    assert_eq!(
        liquibase_fp,
        port_fp,
        "ported op.* platform schema must be byte-identical to original Liquibase \
         after comprehensive catalog introspection; throwaway DBs: {liquibase_db} vs {port_db}\n{}",
        diff_lines(&liquibase_fp, &port_fp)
    );

    drop_database(&admin, &liquibase_db).await;
    drop_database(&admin, &port_db).await;
}

// ---------------------------------------------------------------------------
// The port ROLLBACK gate (design §10, finding `platform-down-rollback-untested`):
// the ported `.down.sql` set + the Platform Down/Rollback commands are shipped
// (`run_down` / `run_rollback`) but no platform test exercised them — the port
// gate above only applies-forward + asserts an idempotent no-op. Rollback was
// covered ONLY for the project profile (`rollback_pg.rs`, synthetic in-test
// migrations), so the platform path that REPLACED Liquibase's `rollback` was
// untested against the REAL ported `.down.sql` files.
//
// This applies the whole ported set, then `run_rollback`'s the SINGLE most-recent
// platform migration (V0057) via its real `.down.sql` under profile=Platform, and
// asserts: the object V0057 created is GONE, the journal reflects the rollback
// (RunReport::Rollback names V0057; status drops to 55 applied with V0057 pending),
// and the rolled-back migration RE-APPLIES forward cleanly (the down was faithful).
//
// V0057 is a self-contained reversible step: its up creates the creator-keyed
// `zeroship.metering_exports` table (+ a guarded GRANT); its down REVOKEs +
// `DROP TABLE`s it. So rolling back exactly one step removes `metering_exports`
// without disturbing the rest of the schema.
//
// Drives the REAL `command::runner::run_rollback` (not a shim, not a spawned
// process) so the compose `migrate` service's rollback path is guarded by the same
// engine + guard. Rollback is destructive ⇒ the run uses `yes = true` (the runner's
// own --yes gate is exercised independently in `cli_platform_pg.rs`).
// ---------------------------------------------------------------------------

#[compio::test]
async fn ported_set_rolls_back_the_last_platform_migration_via_its_down_sql() {
    let conn = pg().await;
    let meta = format!("portrbmeta_{}", token());
    reset(&conn, &meta).await;

    // Apply the WHOLE ported set forward first (the precondition for a rollback).
    let cfg = platform_cfg(&meta, /* yes */ true);
    match run_migrate(&cfg).await.expect("ported set applies under Platform") {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 56, "all 56 ported files applied");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    // The object V0057's `up` creates must exist before we roll it back.
    assert!(
        table_exists(&conn, "zeroship", "metering_exports").await,
        "V0057 materialized zeroship.metering_exports"
    );

    // Roll back exactly the most-recently-applied migration (V0057) via its REAL
    // `.down.sql`. `run_rollback(.., None, Some(1))` is the `down` one-step target.
    let report = run_rollback(&cfg, None, Some(1))
        .await
        .expect("the last platform migration rolls back via its .down.sql");
    match report {
        RunReport::Rollback(outcome) => {
            // V0057 → version 57 → the derived `mig_…` id. Exactly one step undone.
            let expected = zeroship_migrate::migration_id_for_version(57);
            assert_eq!(
                outcome.rolled_back,
                vec![expected.as_str().to_string()],
                "exactly V0057 was rolled back, via its real .down.sql"
            );
            assert!(
                outcome.skipped_irreversible.is_empty(),
                "V0057 is reversible — nothing force-skipped"
            );
        }
        other => panic!("expected Rollback report, got {other:?}"),
    }

    // The dropped object is GONE on the REAL DB (the `.down.sql` actually ran).
    assert!(
        !table_exists(&conn, "zeroship", "metering_exports").await,
        "rolling back V0057 dropped zeroship.metering_exports"
    );

    // The journal reflects the rollback: 55 applied, V0057 now pending again.
    match run_status(&cfg).await.expect("status reads the journal post-rollback") {
        RunReport::Status(status) => {
            assert_eq!(
                status.applied.len(),
                55,
                "the journal shows 55 applied after rolling back V0057"
            );
            assert_eq!(
                status.pending.len(),
                1,
                "V0057 is the single pending migration after its rollback"
            );
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    // Roll-forward heals: re-applying re-runs ONLY V0057 (the down was faithful, so
    // its up re-creates the table). Proves the ported down/up pair round-trips.
    match run_migrate(&cfg).await.expect("re-apply heals the rolled-back step") {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 1, "only V0057 re-applied");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert!(
        table_exists(&conn, "zeroship", "metering_exports").await,
        "re-applying V0057 re-created zeroship.metering_exports"
    );

    // Clean up the journal (leave the schemas; the next run resets them).
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"))
        .await
        .expect("drop journal schema");
}
