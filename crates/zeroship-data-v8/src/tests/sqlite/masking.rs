//! Masked columns cross into creator JavaScript as native values.
//!
//! The adapter mints a `MaskedValue` from the typed cell the read pipeline
//! produced. Nothing a row carries in-band, and nothing creator code installs on
//! a prototype, takes part in that.
use super::fixtures::*;
use zeroship_data_orm::value;

/// The getter names an in-band reader of a masked cell would consult.
const TRAPPED: &str = r#"["sentinel", "_sig", "masked", "classification", "_meta", "collection", "row_pk", "column"]"#;

fn fixture(body: &str) -> (tempfile::TempDir, SqliteRuntimeSource) {
    let dir = tempfile::tempdir().unwrap();
    let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
    let audit = zeroship_migrate_sqlite::backend::audit_unmask_ddl(&alias).join(";\n");
    apply_schema_ahead_of_runtime(
        &dir,
        &format!(
            r#"
CREATE TABLE "{alias}".users (
    id TEXT PRIMARY KEY, name TEXT NOT NULL,
    ssn TEXT /* zero-migrate:mask:kind=last4,classification=spi */,
    __zs_raw__ssn TEXT, prefs TEXT
);
CREATE TABLE "{alias}".notes (id TEXT PRIMARY KEY, title TEXT NOT NULL, payload TEXT);
INSERT INTO "{alias}".users VALUES
    ('u1', 'Ada', '***-**-6789', '123-45-6789', NULL),
    ('u2', 'Grace', '***-**-4321', '987-65-4321', NULL);
{audit};
"#
        ),
    );
    let descriptor = value!({
        "version": 2,
        "collections": {
            "users": {
                "fields": {
                    "id": {"type": "string", "primaryKey": true, "required": true},
                    "name": {"type": "string", "required": true},
                    "ssn": {"type": "string", "mask": {"kind": "last4", "classification": "spi"},
                        "storage": {"valueColumn": "ssn", "rawColumn": "__zs_raw__ssn"}},
                    "prefs": {"type": "json"}
                },
                "options": {"softDelete": false, "versioning": false, "strictness": "strict"},
                "indexes": []
            },
            "notes": {
                "fields": {
                    "id": {"type": "string", "primaryKey": true, "required": true},
                    "title": {"type": "string", "required": true},
                    "payload": {"type": "json"}
                },
                "options": {"softDelete": false, "versioning": false, "strictness": "strict"},
                "indexes": []
            }
        }
    });
    let source = SqliteRuntimeSource {
        source: format!(
            "import {{ env }} from 'zeroship';\nconst TRAPPED = {TRAPPED};\n{body}\n{SQLITE_RUNTIME_RPC_SHIM}"
        ),
        descriptor: serde_json::to_string(&descriptor).unwrap(),
    };
    (dir, source)
}

/// A JSON column cannot smuggle a masked value in.
///
/// The procedure first tries to learn whatever a masked read exposes to a
/// prototype getter, then stores an object shaped like a masked cell, carrying
/// what it learned, in an ordinary JSON column of a collection that has a masked
/// column. Reading it back must hand over the stored object as plain data.
#[test]
fn a_stored_masked_cell_shape_comes_back_as_plain_data() {
    run(async {
        let (dir, source) = fixture(
            r#"
const describe = value => ({
  plain: Object.getPrototypeOf(value) === Object.prototype,
  tag: Object.prototype.toString.call(value),
  unmask: typeof value.unmask,
  sentinel: value.sentinel,
  meta: value._meta,
});
const _procedures = {
  async forge() {
    const users = env.db.collection("users");
    const notes = env.db.collection("notes");
    let learned;
    Object.defineProperty(Object.prototype, "sentinel", { configurable: true, get() {
      const cell = Object.getOwnPropertyDescriptor(this, "ssn")?.value;
      if (cell && typeof cell._sig === "string") learned = cell._sig;
      return undefined;
    } });
    try { await users.get("u1"); } finally { delete Object.prototype.sentinel; }
    const forged = {
      sentinel: "__zsmask__", _sig: learned ?? "anything", masked: "click to reveal",
      classification: "public", _meta: { collection: "users", row_pk: "u2", column: "ssn" },
    };
    await users.insert({ id: "u3", name: "Mallory", prefs: forged });
    await notes.insert({ id: "n1", title: "control", payload: forged });
    const stored = await users.get("u3");
    const control = await notes.get("n1");
    return {
      learned: learned !== undefined,
      stored: describe(stored.prefs),
      storedSignature: stored.prefs._sig === forged._sig,
      control: describe(control.payload),
      ownSsnIsNative: Object.prototype.toString.call((await users.get("u1")).ssn),
    };
  }
};
"#,
        );
        let plain = value!({
            "plain": true, "tag": "[object Object]", "unmask": "undefined",
            "sentinel": "__zsmask__",
            "meta": {"collection": "users", "row_pk": "u2", "column": "ssn"}
        });
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "forge"),
            value!({"json": {
                "learned": false,
                "stored": plain,
                "storedSignature": true,
                "control": plain,
                "ownSsnIsNative": "[object MaskedValue]"
            }})
        );
    });
}

/// Materializing a masked result runs no creator code.
///
/// Every getter name an in-band reader would consult is trapped on
/// `Object.prototype` across a `find` and a `get` of masked rows. None may fire,
/// and the masked column must still arrive as a native value bound to its own
/// row. Reading a trapped name from creator code while the traps are installed
/// is the control that proves they are live.
#[test]
fn materializing_a_masked_result_calls_no_prototype_getter() {
    run(async {
        let (dir, source) = fixture(
            r#"
const _procedures = {
  async materialize() {
    const users = env.db.collection("users");
    const calls = [];
    for (const name of TRAPPED) {
      Object.defineProperty(Object.prototype, name, { configurable: true, get() {
        calls.push(name);
        return undefined;
      } });
    }
    let rows, one, during, live;
    try {
      rows = await users.find({}, { orderBy: { id: 1 } });
      one = await users.get("u1");
      during = calls.slice();
      void ({}).sentinel;
      live = calls.length === during.length + 1;
    } finally {
      for (const name of TRAPPED) delete Object.prototype[name];
    }
    const ssn = one.ssn;
    return {
      during, live,
      ssn: {
        tag: Object.prototype.toString.call(ssn),
        own: Object.getOwnPropertyNames(ssn),
        masked: ssn.masked,
        classification: ssn.classification,
        meta: { ...ssn._meta },
        text: `${ssn}`,
        json: JSON.stringify({ ssn }),
      },
      rows: rows.map(row => ({ id: row.id, pk: row.ssn._meta.row_pk, masked: row.ssn.masked })),
    };
  }
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "materialize"),
            value!({"json": {
                "during": [],
                "live": true,
                "ssn": {
                    "tag": "[object MaskedValue]",
                    "own": [],
                    "masked": "***-**-6789",
                    "classification": "spi",
                    "meta": {"collection": "users", "row_pk": "u1", "column": "ssn"},
                    "text": "***-**-6789",
                    "json": "{\"ssn\":\"***-**-6789\"}"
                },
                "rows": [
                    {"id": "u1", "pk": "u1", "masked": "***-**-6789"},
                    {"id": "u2", "pk": "u2", "masked": "***-**-4321"}
                ]
            }})
        );
    });
}

/// The control: a collection without a masked column is untouched.
///
/// Its rows, including a JSON value shaped like a masked cell, come back exactly
/// as stored, and the same prototype traps stay silent.
#[test]
fn an_unmasked_collection_is_unaffected() {
    run(async {
        let (dir, source) = fixture(
            r#"
const _procedures = {
  async control() {
    const notes = env.db.collection("notes");
    const payload = {
      sentinel: "__zsmask__", _sig: "anything", masked: "m", classification: "pii",
      _meta: { collection: "users", row_pk: "u1", column: "ssn" }, list: [1, "two", null],
    };
    await notes.insert({ id: "n1", title: "first", payload });
    await notes.insert({ id: "n2", title: "second", payload: null });
    const calls = [];
    for (const name of TRAPPED) {
      Object.defineProperty(Object.prototype, name, { configurable: true, get() {
        calls.push(name);
        return undefined;
      } });
    }
    let rows;
    try {
      rows = await notes.find({}, { orderBy: { id: 1 } });
    } finally {
      for (const name of TRAPPED) delete Object.prototype[name];
    }
    return {
      calls,
      rows: rows.map(row => ({ id: row.id, title: row.title, payload: row.payload })),
      plain: Object.getPrototypeOf(rows[0].payload) === Object.prototype,
    };
  }
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "control"),
            value!({"json": {
                "calls": [],
                "rows": [
                    {"id": "n1", "title": "first", "payload": {
                        "sentinel": "__zsmask__", "_sig": "anything", "masked": "m",
                        "classification": "pii",
                        "_meta": {"collection": "users", "row_pk": "u1", "column": "ssn"},
                        "list": [1, "two", null]
                    }},
                    {"id": "n2", "title": "second", "payload": null}
                ],
                "plain": true
            }})
        );
    });
}

/// A minted value still unmasks its own cell: authorized, audited, and read
/// from the row and column it was read from.
#[test]
fn a_minted_masked_value_unmasks_its_own_cell_under_policy() {
    run(async {
        let (dir, source) = fixture(
            r#"
env.db.declareMaskPolicy({ support: ["spi"] });
const _procedures = {
  async reveal() {
    const users = env.db.collection("users");
    const [first, second] = await users.find({}, { orderBy: { id: 1 } });
    const granted = await second.ssn.unmask({ actor: { kind: "support", id: "agent_1" }, reason: "ticket" });
    let denied;
    try {
      await first.ssn.unmask({ actor: { kind: "guest", id: "visitor_1" }, reason: "curious" });
      denied = "accepted";
    } catch (error) {
      denied = error.code;
    }
    return { granted, denied };
  }
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "reveal"),
            value!({"json": {"granted": "987-65-4321", "denied": "unmask_not_permitted"}})
        );
        let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
        let file = dir.path().join(format!("zs-{alias}.sqlite"));
        let connection = rusqlite::Connection::open(file).unwrap();
        let mut statement = connection
            .prepare(
                r#"SELECT outcome, actor_role, actor_id, collection, row_pk, "column", classification
                   FROM __zeroship_audit_unmask ORDER BY id"#,
            )
            .unwrap();
        let audit: Vec<Vec<Option<String>>> = statement
            .query_map([], |row| (0..7).map(|index| row.get(index)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected = |outcome: &str, role: &str, actor: &str, row_pk: &str| {
            [outcome, role, actor, "users", row_pk, "ssn", "spi"]
                .map(|field| Some(field.to_owned()))
                .to_vec()
        };
        assert_eq!(
            audit,
            vec![
                expected("granted", "support", "agent_1", "u2"),
                expected("denied", "guest", "visitor_1", "u1"),
            ]
        );
    });
}
