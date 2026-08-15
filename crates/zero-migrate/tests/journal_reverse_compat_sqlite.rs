//! Backward-compatible SQLite journal-shape proofs for persisted rollback SQL.

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::SqliteBackend;

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(case: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    Paths {
        app: dir.path().join(format!("{case}.sqlite")),
        journal: dir.path().join(format!("{case}.migrations.sqlite")),
        _dir: dir,
    }
}

fn backend(paths: &Paths) -> SqliteBackend {
    SqliteBackend::open(&paths.app, &paths.journal).expect("open sqlite backend")
}

async fn down_column(backend: &SqliteBackend) -> Vec<Option<String>> {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine journal mode");
    backend
        .actor()
        .query("PRAGMA \"_mig\".table_info(schema_migrations)")
        .await
        .expect("read journal table shape")
        .into_iter()
        .find(|row| row.get(1).and_then(Option::as_deref) == Some("down"))
        .expect("schema_migrations has a down column")
}

#[compio::test]
async fn fresh_journal_has_nullable_down_column() {
    let paths = paths("fresh_down");
    let backend = backend(&paths);

    backend
        .ensure_journal_sqlite()
        .await
        .expect("bootstrap fresh journal");

    let column = down_column(&backend).await;
    assert_eq!(column[2].as_deref(), Some("TEXT"), "down stores SQL text");
    assert_eq!(column[3].as_deref(), Some("0"), "down must stay nullable");
}

#[compio::test]
async fn bootstrap_adds_nullable_down_to_legacy_journal() {
    let paths = paths("legacy_down");
    {
        let journal = rusqlite::Connection::open(&paths.journal).expect("open legacy journal");
        journal
            .execute_batch(
            "CREATE TABLE schema_migrations (\
                event_seq  INTEGER PRIMARY KEY AUTOINCREMENT, \
                event_kind TEXT NOT NULL, \
                version    TEXT NOT NULL, \
                name       TEXT NOT NULL, \
                checksum   TEXT NOT NULL, \
                \"at\"       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                \"by\"       TEXT NOT NULL, \
                exec_ms    INTEGER, \
                phase      TEXT, \
                outcome    TEXT, \
                kind       TEXT)",
            )
            .expect("create pre-upgrade journal shape");
    }

    let backend = backend(&paths);

    backend
        .ensure_journal_sqlite()
        .await
        .expect("upgrade legacy journal");

    let column = down_column(&backend).await;
    assert_eq!(column[2].as_deref(), Some("TEXT"), "down stores SQL text");
    assert_eq!(column[3].as_deref(), Some("0"), "legacy rows require NULL down");
}
