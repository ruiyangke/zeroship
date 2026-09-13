import { test } from "node:test";
import assert from "node:assert/strict";
import { bindingMap, buildEntryFixture } from "./helpers/server-entry-fixture.js";

test("binding targets retain callable and metadata identity", async (t) => {
  const fixture = await buildEntryFixture(t, {
    bindings: bindingMap([
      { sourceFile: "./actions.mjs", exportName: "list", wireId: "todos.list" },
      { sourceFile: "./actions.mjs", exportName: "add", wireId: "__proto__" },
      { sourceFile: "./actions.mjs", exportName: "quoted export", wireId: 'with "quotes"' },
    ]),
    files: {
      "user.mjs": `
        export const unlisted = () => 'private';
        export default { rpc: { 'todos.list': () => 'replaced', retained: () => 'retained' } };
      `,
      "actions.mjs": `
        export const list = Object.assign(() => 'list', { config: Object.freeze({ kind: 'query' }) });
        export const add = Object.assign(() => 'add', { config: Object.freeze({ kind: 'mutation' }) });
        list.identity = list;
        add.identity = add;
        const quoted = () => 'quoted';
        export { quoted as "quoted export" };
      `,
    },
  });
  const { rpc } = (await fixture.load()).default;
  assert.equal(rpc["todos.list"](), "list");
  assert.equal(rpc["todos.list"], rpc["todos.list"].identity);
  assert.equal(rpc["todos.list"].config.kind, "query");
  assert.equal(rpc.__proto__, rpc.__proto__.identity);
  assert.equal(rpc.__proto__.config.kind, "mutation");
  assert.ok(Object.hasOwn(rpc, "__proto__"));
  assert.equal(rpc['with "quotes"'](), "quoted");
  assert.equal(rpc.retained(), "retained");
  assert.equal(Object.hasOwn(rpc, "unlisted"), false);
  const imports = fixture.artifact.inputs["entry.mjs"].imports;
  assert.deepEqual(imports.filter(entry => entry.path === "actions.mjs").map(entry => entry.kind), [
    "import-statement",
  ]);
});

test("binding modules initialize in deterministic file order", async (t) => {
  const rows = [
    { sourceFile: "./b.mjs", exportName: "b" },
    { sourceFile: "./a.mjs", exportName: "a" },
  ];
  for (const bindings of [bindingMap(rows), bindingMap([...rows].reverse())]) {
    const fixture = await buildEntryFixture(t, {
      bindings,
      files: {
        "user.mjs": "import { order } from './state.mjs'; export default { rpc: { inspect: () => order } };",
        "state.mjs": "export const order = [];",
        "a.mjs": "import { order } from './state.mjs'; order.push('a'); export const a = () => 'A';",
        "b.mjs": "import { order } from './state.mjs'; order.push('b'); export const b = () => 'B';",
      },
    });
    const { rpc } = (await fixture.load()).default;
    assert.deepEqual(rpc.inspect(), ["a", "b"]);
    assert.equal(rpc.a(), "A");
    assert.equal(rpc.b(), "B");
    assert.deepEqual(
      fixture.artifact.inputs["entry.mjs"].imports
        .filter(entry => entry.path === "a.mjs" || entry.path === "b.mjs")
        .map(entry => entry.path),
      ["a.mjs", "b.mjs"],
    );
  }
});

test("empty bindings discover named exports", async (t) => {
  const fixture = await buildEntryFixture(t, {
    bindings: new Map(),
    files: { "user.mjs": "export const named = () => 'discovered';" },
  });
  const entry = (await fixture.load()).default;
  assert.deepEqual(Object.keys(entry.rpc), ["named"]);
  assert.equal(entry.rpc.named(), "discovered");
});
