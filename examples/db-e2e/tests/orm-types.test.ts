import assert from "node:assert/strict";
import { resolve } from "node:path";
import ts from "typescript";
import { test } from "vitest";

const root = resolve(import.meta.dirname, "..");
const configPath = resolve(root, "tsconfig.json");
const config = ts.readConfigFile(configPath, ts.sys.readFile);
assert.equal(config.error, undefined);
const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, root);
assert.deepEqual(parsed.errors, []);

function messages(diagnostics: readonly ts.Diagnostic[]): string[] {
  return diagnostics.map(diagnostic => {
    const location = diagnostic.file && diagnostic.start !== undefined
      ? `${diagnostic.file.fileName}:${diagnostic.file.getLineAndCharacterOfPosition(diagnostic.start).line + 1}: `
      : "";
    return location + ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n");
  });
}

test("the app compiles against the packaged ORM declarations", () => {
  assert.ok(parsed.fileNames.length > 0);
  const program = ts.createProgram(parsed.fileNames, { ...parsed.options, noEmit: true });
  assert.ok(program.getSourceFile(resolve(root, "src/server.ts")));
  assert.ok(program.getSourceFile(resolve(root, "generated/zeroship/env.db.ts")));
  assert.deepEqual(messages(ts.getPreEmitDiagnostics(program)), []);
});

test("packaged transaction inputs preserve declared identity and relation types", () => {
  const file = resolve(root, "src/__transaction_type_control.ts");
  const source = `
import { env } from "zeroship";
import type {} from "../generated/zeroship/env.db";
async function useTransaction() {
  return env.db.transaction(async tx => {
    const task = await tx.tasks.find().with({ owner: true, workspace: true }).unique();
    const scalar: string = task.ownerId;
    const related: string | undefined = task.owner?.fullName;
    await tx.tasks.update(task.id, { title: "Renamed" });
    // @ts-expect-error Foreign keys keep their declared scalar storage.
    await tx.tasks.update(task.id, { ownerId: 42 });
    // @ts-expect-error Relations are selected by declared edge name.
    tx.tasks.find().with({ ownerId: true });
    // @ts-expect-error Loaded target fields retain their declared types.
    const wrong: number | undefined = task.owner?.fullName;
    return { scalar, related, wrong };
  });
}
void useTransaction;
`;
  const options = { ...parsed.options, noEmit: true };
  const host = ts.createCompilerHost(options);
  const getSourceFile = host.getSourceFile.bind(host);
  host.getSourceFile = (name, version, ...args) => name === file
    ? ts.createSourceFile(file, source, version, true)
    : getSourceFile(name, version, ...args);
  const program = ts.createProgram([file], options, host);
  assert.ok(program.getSourceFile(file));
  assert.deepEqual(messages(ts.getPreEmitDiagnostics(program)), []);
});
