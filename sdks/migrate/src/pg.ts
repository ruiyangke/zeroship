// `@zeroship/migrate/pg` — the privileged Postgres vendor authoring surface.
//
// Boundary: this subpath is intentionally importable so platform/operator
// migrations can author closed vendor IR. It is not the security gate. Confined
// creator deploy/load paths reject every op emitted here with VENDOR_OP_DENIED;
// operator callers must pass an explicit platform/trusted capability to validate
// and lower it. Do not add a client-side "trust" global here: migration JS is
// untrusted authoring code, so a spoofable flag would only create a false gate.
//
// This is intentionally published as a separate subpath and is not re-exported
// from `@zeroship/migrate`. Each method records the same op payload shape as the
// engine-embedded recorder twin (`crates/zeroship-migrate/src/frontend/migrate_ops.js`).

import type { PgTableHandle, TableOptions } from "./types.js";
import type {
  FuncArg,
  FuncLanguage,
  FuncVolatility,
  GrantTarget,
  Privilege,
} from "./generated/ir.js";
import {
  __makePgTableHandle,
  __pgDomain,
  __pgExtension,
  __pgPush,
  __pgRole,
  __pgSchema,
  __pgSequence,
} from "./ops.js";

type Node = Record<string, unknown>;

export type {
  FuncArg,
  FuncArgMode,
  FuncLanguage,
  FuncVolatility,
  GrantTarget,
  PolicyCmd,
  PgExtractField,
  Privilege,
} from "./generated/ir.js";

export type {
  AlterSequenceArgs,
  CreateDomainArgs,
  CreateSequenceArgs,
  DomainCheckFn,
  DomainHandle,
  DomainValueBuilder,
  DroppedExtensionHandle,
  DroppedRoleHandle,
  DroppedSchemaHandle,
  DropDomainArgs,
  DropSequenceArgs,
  ExtensionCreateArgs,
  ExtensionDropArgs,
  ExtensionHandle,
  CheckBuilderWithPg,
  PgCheckDef,
  PgCheckExprFn,
  PgCheckRef,
  PgConstraintRef,
  PgIndexAdd,
  PgIndexDropArgs,
  PgIndexElement,
  PgIndexMethod,
  PgIndexRef,
  PgTableHandle,
  PolicyCreateArgs,
  PolicyDropArgs,
  PolicyRef,
  RoleCreateArgs,
  RoleDropArgs,
  RoleHandle,
  RoleSetOptionsArgs,
  SchemaCreateArgs,
  SchemaDropArgs,
  SchemaHandle,
  SequenceHandle,
  SequenceOwnedBy,
} from "./types.js";

export interface DropOwnedByArgs {
  roles: string[];
}

export interface GrantArgs {
  privileges: Privilege[];
  on: GrantTarget;
  to: string[];
  withGrantOption?: boolean;
}

export interface RevokeArgs {
  privileges: Privilege[];
  on: GrantTarget;
  from: string[];
}

export interface CreateFunctionArgs {
  name: string;
  schema?: string;
  args?: FuncArg[];
  returns: string;
  language: FuncLanguage;
  replace?: boolean;
  volatility?: FuncVolatility;
  body: string;
}

export interface DropFunctionArgs {
  name: string;
  schema?: string;
  argTypes?: string[];
  ifExists?: boolean;
}

export interface PgRawArgs {
  sql: string;
  reason: string;
}

function structuredError(code: string, message: string, extra?: Record<string, unknown>): Error {
  const err = new Error(message) as Error & Record<string, unknown>;
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

function compact<T extends Record<string, unknown>>(obj: T): T {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

function requireString(v: unknown, what: string): asserts v is string {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}

function record(op: Node): Node {
  return __pgPush(compact(op));
}

export const domain = __pgDomain;
export const schema = __pgSchema;
export const extension = __pgExtension;
export const role = __pgRole;
export const sequence = __pgSequence;

export function pgTable(name: string, opts?: TableOptions): PgTableHandle {
  return __makePgTableHandle(name, opts);
}

export function dropOwnedBy(args: DropOwnedByArgs): Node {
  if (!Array.isArray(args.roles)) {
    throw structuredError("OP_INVALID", "dropOwnedBy({ roles }): roles must be an array");
  }
  return record({ op: "dropOwnedBy", roles: args.roles });
}

export function grant(args: GrantArgs): Node {
  if (!Array.isArray(args.privileges) || args.privileges.length === 0) {
    throw structuredError("OP_INVALID", "grant({ privileges }): privileges must be a non-empty array");
  }
  if (args.on === null || typeof args.on !== "object") {
    throw structuredError("OP_INVALID", "grant({ on }): on must be a target object");
  }
  if (!Array.isArray(args.to) || args.to.length === 0) {
    throw structuredError("OP_INVALID", "grant({ to }): to must be a non-empty array");
  }
  return record({
    op: "grant",
    privileges: args.privileges,
    on: args.on,
    to: args.to,
    withGrantOption: args.withGrantOption,
  });
}

export function revoke(args: RevokeArgs): Node {
  if (!Array.isArray(args.privileges) || args.privileges.length === 0) {
    throw structuredError("OP_INVALID", "revoke({ privileges }): privileges must be a non-empty array");
  }
  if (args.on === null || typeof args.on !== "object") {
    throw structuredError("OP_INVALID", "revoke({ on }): on must be a target object");
  }
  if (!Array.isArray(args.from) || args.from.length === 0) {
    throw structuredError("OP_INVALID", "revoke({ from }): from must be a non-empty array");
  }
  return record({
    op: "revoke",
    privileges: args.privileges,
    on: args.on,
    from: args.from,
  });
}

export function createFunction(args: CreateFunctionArgs): Node {
  requireString(args.name, "createFunction({ name })");
  requireString(args.returns, "createFunction({ returns })");
  requireString(args.language, "createFunction({ language })");
  requireString(args.body, "createFunction({ body })");
  return record({
    op: "createFunction",
    name: args.name,
    schema: args.schema,
    args: args.args,
    returns: args.returns,
    language: args.language,
    replace: args.replace,
    volatility: args.volatility,
    body: args.body,
  });
}

export function dropFunction(args: DropFunctionArgs): Node {
  requireString(args.name, "dropFunction({ name })");
  return record({
    op: "dropFunction",
    name: args.name,
    schema: args.schema,
    argTypes: args.argTypes,
    ifExists: args.ifExists,
  });
}

export function raw(args: PgRawArgs): Node {
  requireString(args.sql, "raw({ sql })");
  requireString(args.reason, "raw({ reason })");
  return record({
    op: "pgRaw",
    sql: args.sql,
    reason: args.reason,
  });
}
