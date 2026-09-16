//! The generator binds the journal's code-owned identifiers into the reserved
//! `__zeroship_workflow_` namespace AFTER compilation, because the authoring DSL
//! refuses a reserved name from a creator envelope. That post-pass is the one
//! place a string literal could be rewritten by accident, so it is checked here,
//! beside the generator that runs it.

use std::process::Command;

#[test]
fn schema_binding_preserves_literals_and_includes_compiler_generated_names() {
    let output = Command::new("node")
        .args(["--input-type=module", "--eval"])
        .arg(
            r#"
import assert from 'node:assert/strict';
import { bindOwnedNames } from './schema/names.mjs';
const identifiers = new Set(['journal']);
const columns = new Set(['id', 'body']);
const sql = `CREATE TABLE "journal" (id TEXT, body TEXT DEFAULT 'journal '' "journal" CREATE INDEX "literal"', CONSTRAINT "journal_pkey" PRIMARY KEY (id, body), FOREIGN KEY (id) REFERENCES journal(id));
CREATE INDEX IF NOT EXISTS "journal_id_idx" ON "journal" (id);`;
const bound = bindOwnedNames(sql, { identifiers, columns });
assert.ok(bound.includes(`DEFAULT 'journal '' "journal" CREATE INDEX "literal"'`));
assert.ok(bound.includes('REFERENCES "__zeroship_workflow_journal"(id)'));
assert.ok(bound.includes('CONSTRAINT "__zeroship_workflow_journal_pkey"'));
assert.ok(bound.includes('INDEX IF NOT EXISTS "__zeroship_workflow_journal_id_idx"'));
assert.ok(bound.includes('ON "__zeroship_workflow_journal" (id)'));
for (const name of ['journal', 'journal_pkey', 'journal_id_idx']) {
    assert.throws(() => bindOwnedNames(sql, { identifiers, columns: new Set([name]) }), /collides with a column/);
}
assert.throws(() => bindOwnedNames(sql, { identifiers: new Set(['omitted']), columns }), /omitted owned identifier/);
assert.throws(() => bindOwnedNames('', { identifiers: new Set(), columns }), /no owned identifiers/);
assert.throws(() => bindOwnedNames('', { identifiers: new Set(['a'.repeat(64)]), columns }), /PostgreSQL limit/);
assert.throws(() => bindOwnedNames('', { identifiers: new Set(['untrusted"']), columns }), /invalid workflow owned name/);
"#,
        )
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run node against the owned-name binder");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
