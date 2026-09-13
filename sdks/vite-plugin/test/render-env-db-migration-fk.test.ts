import { test } from "node:test";
import assert from "node:assert/strict";
import { resolve } from "node:path";
import ts from "typescript";
import * as sdk from "@zeroship/db";
import { normalizeSchema } from "@zeroship/bootstrap/install-schema";
import { renderGeneratedEnvDb, type RuntimeDescriptor } from "../src/gen-types/render-env-db.js";
import { fieldDefToDto } from "../src/gen-types/manual.js";

const descriptor: RuntimeDescriptor = {
  version: 2,
  collections: {
    users: { fields: {
      id: { type: "string", required: true, primaryKey: true },
      account_key: { type: "string", required: true, unique: true },
      email: { type: "string", required: true },
    } },
    integerUsers: { fields: { id: { type: "integer", required: true, primaryKey: true }, email: { type: "string", required: true } } },
    bigintUsers: { fields: { id: { type: "bigInt", required: true, primaryKey: true }, email: { type: "string", required: true } } },
    todos: { fields: {
      id: { type: "string", required: true, primaryKey: true },
      userId: { type: "string", required: true, refTarget: "users", refColumn: "account_key", relation: "user" },
      integerUserId: { type: "integer", required: true, refTarget: "integerUsers", refColumn: "id", relation: "integerUser" },
      bigintUserId: { type: "bigInt", required: true, refTarget: "bigintUsers", refColumn: "id", relation: "bigintUser" },
      unnamedUserId: { type: "string", refTarget: "users", refColumn: "id" },
      title: { type: "string", required: true },
    } },
  },
};

test("generated builders preserve named edges, scalar references, and ordinary fields", () => {
  const source = renderGeneratedEnvDb(descriptor) + "\nexport { schema };\n";
  const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } });
  const exports: { schema?: Record<string, Record<string, sdk.TypeBuilder>> } = {};
  new Function("require", "exports", compiled.outputText)((name: string) => {
    assert.equal(name, "@zeroship/db");
    return sdk;
  }, exports);
  assert.ok(exports.schema?.todos);
  const normalized = normalizeSchema(exports.schema.todos as Parameters<typeof normalizeSchema>[0]);
  assert.deepEqual(normalized.integerUserId, { type: "integer", required: true, refTarget: "integerUsers", refColumn: "id", relation: "integerUser" });
  assert.deepEqual(normalized.bigintUserId, { type: "bigInt", required: true, refTarget: "bigintUsers", refColumn: "id", relation: "bigintUser" });
  assert.deepEqual(exports.schema.todos.userId.toFieldDef(), {
    type: "ref", required: true, refTarget: "users", refColumn: "account_key", relation: "user",
  });
  assert.deepEqual(exports.schema.todos.integerUserId.toFieldDef(), { type: "integer", required: true, refTarget: "integerUsers", refColumn: "id", relation: "integerUser" });
  assert.deepEqual(exports.schema.todos.bigintUserId.toFieldDef(), { type: "bigInt", required: true, refTarget: "bigintUsers", refColumn: "id", relation: "bigintUser" });
  assert.equal(exports.schema.integerUsers.id.toFieldDef().type, "integer");
  assert.equal(exports.schema.todos.unnamedUserId.toFieldDef().relation, undefined);
  assert.deepEqual(exports.schema.todos.title.toFieldDef(), { type: "string", required: true });
});

test("generated relation names and target row types compile without widening", () => {
  const file = resolve(import.meta.dirname, "__generated-relation-contract.ts");
  const source = renderGeneratedEnvDb(descriptor) + `
declare const db: Db<typeof schema>;
async function useRelations() {
  const rows = await db.todos.find().with({ user: true });
  if (rows.data) {
    const fk: string = rows.data[0].userId;
    const email: string | undefined = rows.data[0].user?.email;
    // @ts-expect-error Relation target fields retain their declared type.
    const wrong: number = rows.data[0].user!.email;
    void [fk, email, wrong];
  }
  const numeric = await db.todos.find().with({ integerUser: true, bigintUser: true });
  if (numeric.data) {
    const integer: number = numeric.data[0].integerUserId;
    const bigint: number | bigint = numeric.data[0].bigintUserId;
    const integerTarget: number | undefined = numeric.data[0].integerUser?.id;
    const bigintTarget: number | bigint | undefined = numeric.data[0].bigintUser?.id;
    const email: string | undefined = numeric.data[0].bigintUser?.email;
    // @ts-expect-error Integer references do not become text.
    const wrongInteger: string = numeric.data[0].integerUserId;
    // @ts-expect-error Bigint target identities do not become text.
    const wrongBigint: string | undefined = numeric.data[0].bigintUser?.id;
    void [integer, bigint, integerTarget, bigintTarget, email, wrongInteger, wrongBigint];
  }
  // @ts-expect-error Scalar FK names are not relation edges.
  db.todos.find().with({ userId: true });
  // @ts-expect-error References without a relation name are not edges.
  db.todos.find().with({ unnamedUserId: true });
}
void useRelations;
`;
  const options: ts.CompilerOptions = {
    strict: true, noEmit: true, skipLibCheck: true, target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext, moduleResolution: ts.ModuleResolutionKind.Bundler,
    types: [],
  };
  const host = ts.createCompilerHost(options);
  const getSourceFile = host.getSourceFile.bind(host);
  host.getSourceFile = (name, version, ...args) => name === file
    ? ts.createSourceFile(file, source, version, true)
    : getSourceFile(name, version, ...args);
  const program = ts.createProgram([file, resolve(import.meta.dirname, "../../types/globals.d.ts")], options, host);
  const diagnostics = ts.getPreEmitDiagnostics(program);
  assert.deepEqual(diagnostics.map(diagnostic => ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n")), []);
});

test("manual descriptor mapping carries relation names into the schema engine", () => {
  const dto = fieldDefToDto("todos", "userId", {
    type: "ref", refTarget: "users", refColumn: "account_key", relation: "user",
  });
  assert.equal(dto.references, "users");
  assert.equal(dto.referenceColumn, "account_key");
  assert.equal(dto.relation, "user");
  for (const [builder, type] of [[sdk.t.integer(), "integer"], [sdk.t.bigInt(), "bigInt"]] as const) {
    const field = builder.references("users", { column: "account_key", relation: "user" }).toFieldDef();
    const numeric = fieldDefToDto("todos", "userId", field);
    assert.equal(numeric.type, type);
    assert.equal(numeric.references, "users");
    assert.equal(numeric.relation, "user");
  }
  const ordinary = fieldDefToDto("todos", "title", { type: "string" });
  assert.equal(ordinary.relation, undefined);
});

test("unsupported reference storage cannot generate a text reference", () => {
  assert.throws(() => renderGeneratedEnvDb({ collections: {
    invalid: { fields: { owner: { type: "boolean", refTarget: "users", relation: "user" } } },
  } }), /reference storage/);
});
