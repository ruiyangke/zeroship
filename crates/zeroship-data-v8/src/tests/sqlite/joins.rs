use super::fixtures::*;
use zeroship_data_sql::{value, value::Value};

#[test]
fn sdk_join_executes_through_v8_and_preserves_transaction_scope() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        apply_schema_ahead_of_runtime(
            &dir,
            &format!(
                "CREATE TABLE \"default\".posts ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL, payload BLOB, counter INTEGER DEFAULT 7, nickname TEXT, score REAL);"
            ),
        );
        let descriptor: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../zeroship-data-orm/tests/fixtures/schema.runtime.json"
        )))
        .unwrap();
        let source = sqlite_runtime_source(
            "posts",
            &descriptor["collections"]["posts"]["fields"],
            r#"
const eq = (a, b) => ({ op: "eq", left: a.expression, right: b?.expression ?? { value: b } });
const _procedures = {
  async joins() {
    for (const document of [{ title: "parent", nickname: "child" }, { title: "child" }, { title: "orphan", nickname: "absent" }]) {
      const result = await env.db.posts.insert(document);
      if (result.error) throw result.error;
    }
    const o = env.db.posts.as("o");
    const c = env.db.posts.as("c");
    const query = env.db.from(o).leftJoin(c, eq(o.columns.nickname, c.columns.title))
      .where(eq(o.columns.title, "parent")).select({ order: o.row(), child: c.optionalRow() });
    const result = await query.all();
    if (result.error) throw result.error;
    const orphan = await env.db.from(o).leftJoin(c, eq(o.columns.nickname, c.columns.title))
      .where(eq(o.columns.title, "orphan")).select({ order:o.row(), child:c.optionalRow() }).all();
    if (orphan.error) throw orphan.error;
    let escaped;
    const txResult = await env.db.transaction(async tx => {
      await tx.posts.insert({ title: "inside", nickname: "inside" });
      const a = tx.posts.as("a");
      const b = tx.posts.as("b");
      escaped = tx.from(a).innerJoin(b, eq(a.columns.nickname, b.columns.title))
        .where(eq(a.columns.title, "inside")).select({ left:a.row(), right:b.row() });
      return (await escaped.all()).map(row => [row.left.title, row.right.title]);
    });
    if (txResult.error) throw txResult.error;
    let expired = false;
    try { await escaped.all(); } catch (error) { expired = error.code === "TRANSACTION_SCOPE_EXPIRED"; }
    return { child:result.data[0].child.title, parent:result.data[0].order.title, missing:orphan.data[0].child, tx:txResult.data, expired };
  }
};
"#,
        );
        let response = dispatch_sqlite_runtime(&dir, &source, "joins");
        assert_eq!(
            response,
            value!({"json":{"child":"child", "parent":"parent", "missing":null, "tx":[["inside","inside"]], "expired":true}})
        );
    });
}
