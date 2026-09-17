use super::fixtures::*;
use zeroship_data_orm::value;

fn fixture(body: &str) -> (tempfile::TempDir, SqliteRuntimeSource) {
    let dir = tempfile::tempdir().unwrap();
    apply_schema_ahead_of_runtime(
        &dir,
        &format!(
            r#"
CREATE TABLE "{LOCAL_DEV_APP_ID}".users (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, retired TEXT,
    ssn TEXT /* zero-migrate:mask:kind=last4,classification=spi */,
    __zs_raw__ssn TEXT, tally INTEGER, payload BLOB, moment TEXT
);
CREATE TABLE "{LOCAL_DEV_APP_ID}".posts (
    id TEXT PRIMARY KEY, title TEXT NOT NULL, author_id TEXT, editor_id TEXT,
    hidden_owner TEXT,
    masked_owner TEXT /* zero-migrate:mask:kind=last4,classification=pii */,
    __zs_raw__masked_owner TEXT
);
INSERT INTO "{LOCAL_DEV_APP_ID}".users VALUES
    ('u1', 'Ada', NULL, '***-**-6789', '123-45-6789', 9007199254740993, X'0001FF', '2026-09-12T12:00:00Z'),
    ('u2', 'Deleted', '2026-09-12T12:00:00Z', NULL, NULL, NULL, NULL, NULL);
INSERT INTO "{LOCAL_DEV_APP_ID}".posts VALUES
    ('p1', 'present', 'u1', 'u1', 'u1', '**', 'u1'),
    ('p2', 'empty', NULL, NULL, NULL, NULL, NULL),
    ('p3', 'deleted', 'u2', NULL, NULL, NULL, NULL),
    ('p4', 'missing', 'absent', NULL, NULL, NULL, NULL);
"#
        ),
    );
    let descriptor = value!({
        "version":2,
        "collections": {
            "users": {
                "fields": {
                    "id":{"type":"string", "primaryKey":true, "required":true},
                    "name":{"type":"string", "required":true},
                    "tally":{"type":"bigInt"},
                    "payload":{"type":"bytes"},
                    "moment":{"type":"timestamp"},
                    "retired":{"type":"timestamp", "softDelete":true, "writable":false,
                        "assign":{"by":"now", "on":"delete"}},
                    "ssn":{"type":"string", "mask":{"kind":"last4", "classification":"spi"},
                        "storage":{"valueColumn":"ssn", "rawColumn":"__zs_raw__ssn"}}
                },
                "options":{"softDelete":true, "versioning":false, "strictness":"strict"},
                "indexes":[]
            },
            "posts": {
                "fields": {
                    "id":{"type":"string", "primaryKey":true, "required":true},
                    "title":{"type":"string", "required":true},
                    "author_id":{"type":"string", "refTarget":"users", "refColumn":"id", "relation":"author"},
                    "editor_id":{"type":"string", "refTarget":"users", "refColumn":"id", "relation":"editor"},
                    "hidden_owner":{"type":"string", "refTarget":"users", "refColumn":"id", "relation":"hiddenOwner",
                        "readable":false, "projectable":false, "filterable":false},
                    "masked_owner":{"type":"string", "refTarget":"users", "refColumn":"id", "relation":"maskedOwner",
                        "mask":{"kind":"last4", "classification":"pii"},
                        "storage":{"valueColumn":"masked_owner", "rawColumn":"__zs_raw__masked_owner"}}
                },
                "options":{"softDelete":false, "versioning":false, "strictness":"strict"},
                "indexes":[]
            }
        }
    });
    let source = SqliteRuntimeSource {
        source: format!("import {{ env }} from 'zeroship';\n{body}\n{SQLITE_RUNTIME_RPC_SHIM}"),
        descriptor: serde_json::to_string(&descriptor).unwrap(),
    };
    (dir, source)
}

#[test]
fn raw_collection_relations_hydrate_projection_and_preserve_child_masking() {
    run(async {
        let (dir, source) = fixture(
            r#"
const _procedures = {
  async relations() {
    const posts = env.db.collection("posts");
    const rows = await posts.find({}, {
      select: ["title", "author_id"], orderBy: { id: 1 }, with: { author: true }
    });
    const projected = await posts.find({id: "p1"}, {
      select: ["title"], with: { author: true }
    });
    return { rows: rows.map(row => ({
      keys: Object.keys(row).sort(), title: row.title, author_id: row.author_id,
      author: row.author ? {
        id: row.author.id, name: row.author.name,
        tally: row.author.tally.toString(), nativeBigInt: typeof row.author.tally === "bigint",
        payload: Array.from(row.author.payload), nativeBytes: row.author.payload instanceof Uint8Array,
        moment: new Date(row.author.moment).toISOString(), nativeTimestamp: Number.isInteger(row.author.moment),
        masked: row.author.ssn.masked, metadata: row.author.ssn._meta,
        nativeMask: typeof row.author.ssn.unmask === "function",
        exposedRaw: Object.hasOwn(row.author, "__zs_raw__ssn")
      } : null
    })), projected: {
      keys: Object.keys(projected[0]).sort(),
      author: projected[0].author?.name ?? null,
      exposedForeignKey: Object.hasOwn(projected[0], "author_id")
    }};
  }
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "relations"),
            value!({"json":{"rows":[
                {"keys":["author", "author_id", "title"], "title":"present", "author_id":"u1", "author":{
                    "id":"u1", "name":"Ada", "masked":"***-**-6789",
                    "tally":"9007199254740993", "nativeBigInt":true,
                    "payload":[0,1,255], "nativeBytes":true,
                    "moment":"2026-09-12T12:00:00.000Z", "nativeTimestamp":true,
                    "metadata":{"collection":"users", "row_pk":"u1", "column":"ssn"},
                    "nativeMask":true, "exposedRaw":false
                }},
                {"keys":["author", "author_id", "title"], "title":"empty", "author_id":null, "author":null},
                {"keys":["author", "author_id", "title"], "title":"deleted", "author_id":"u2", "author":null},
                {"keys":["author", "author_id", "title"], "title":"missing", "author_id":"absent", "author":null}
            ], "projected":{"keys":["author", "title"], "author":"Ada", "exposedForeignKey":false}}})
        );
    });
}

#[test]
fn raw_collection_relations_share_the_callback_transaction_and_rollback() {
    run(async {
        let (dir, source) = fixture(
            r#"
const _procedures = {
  async transactionRelations() {
    let observed;
    const result = await env.db.transaction(async () => {
      await env.db.collection("users").insert({ id: "inside-user", name: "Uncommitted" });
      const posts = env.db.collection("posts");
      await posts.insert({ id: "inside-post", title: "Uncommitted post", author_id: "inside-user", editor_id: "u1" });
      const rows = await posts.find({ id: "inside-post" }, {
        select: ["title", "author_id", "editor_id"],
        with: { author: true, editor: true }
      });
      observed = {
        title: rows[0].title, author: rows[0].author?.name ?? null, editor: rows[0].editor?.name ?? null,
        authorId: rows[0].author_id, editorId: rows[0].editor_id
      };
      throw new Error("rollback relation fixture");
    });
    return {
      observed, rolledBack: result.error?.message === "rollback relation fixture",
      remainingParents: await env.db.collection("posts").count({ id: "inside-post" }),
      remainingTargets: await env.db.collection("users").count({ id: "inside-user" })
    };
  }
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "transactionRelations"),
            value!({"json":{
                "observed":{"title":"Uncommitted post", "author":"Uncommitted", "editor":"Ada",
                    "authorId":"inside-user", "editorId":"u1"},
                "rolledBack":true, "remainingParents":0, "remainingTargets":0
            }})
        );
    });
}

#[test]
fn raw_collection_relations_reject_undeclared_and_protected_reference_access() {
    run(async {
        let (dir, source) = fixture(
            r#"
const _procedures = {
  async invalidRelations() {
    const posts = env.db.collection("posts");
    const codes = [];
    for (const withSpec of [
      { title: true }, { author: { field: "author_id" } },
      { hiddenOwner: true }, { maskedOwner: true }, { author_id: true }, { author: false }
    ]) {
      try {
        await posts.find({ id: "p1" }, { select: ["title"], with: withSpec });
        codes.push("accepted");
      } catch (error) { codes.push(error.code ?? "missing_code"); }
    }
    return codes;
  }
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "invalidRelations"),
            value!({"json":[
                "unknown_relation", "WITH_UNSUPPORTED_VALUE",
                "invalid_relation", "invalid_relation", "unknown_relation", "WITH_UNSUPPORTED_VALUE"
            ]})
        );
    });
}
