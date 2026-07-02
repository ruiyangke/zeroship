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

import type { Expr, ExprChain, ExprFn } from "./types.js";
import type {
  FuncArg,
  FuncLanguage,
  FuncVolatility,
  GrantTarget,
  PolicyCmd,
  Privilege,
} from "./generated/ir.js";
import { __pgPush, __pgResolveExpr } from "./ops.js";

type Node = Record<string, unknown>;

export type {
  FuncArg,
  FuncArgMode,
  FuncLanguage,
  FuncVolatility,
  GrantTarget,
  PolicyCmd,
  Privilege,
} from "./generated/ir.js";

export interface CreateSchemaArgs {
  name: string;
  ifNotExists?: boolean;
  authorization?: string;
}

export interface DropSchemaArgs {
  name: string;
  ifExists?: boolean;
  cascade?: boolean;
}

export interface CreateExtensionArgs {
  name: string;
  ifNotExists?: boolean;
  schema?: string;
}

export interface DropExtensionArgs {
  name: string;
  ifExists?: boolean;
}

export interface CreateRoleArgs {
  name: string;
  login?: boolean;
  password?: string;
  bypassRls?: boolean;
  createRole?: boolean;
  createDb?: boolean;
  superuser?: boolean;
  inRole?: string[];
  setSearchPath?: string[];
  ifNotExists?: boolean;
}

export interface AlterRoleArgs {
  name: string;
  setSearchPath?: string[];
  resetSearchPath?: boolean;
}

export interface DropRoleArgs {
  name: string;
  ifExists?: boolean;
}

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

export interface CreatePolicyArgs {
  name: string;
  table: string;
  schema?: string;
  for?: PolicyCmd;
  to?: string[];
  using: ExprFn | ExprChain | Expr;
  withCheck?: ExprFn | ExprChain | Expr;
}

export interface DropPolicyArgs {
  name: string;
  table: string;
  schema?: string;
  ifExists?: boolean;
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

export interface PgNamespace {
  createSchema(args: CreateSchemaArgs): Node;
  dropSchema(args: DropSchemaArgs): Node;
  createExtension(args: CreateExtensionArgs): Node;
  dropExtension(args: DropExtensionArgs): Node;
  createRole(args: CreateRoleArgs): Node;
  alterRole(args: AlterRoleArgs): Node;
  dropRole(args: DropRoleArgs): Node;
  dropOwnedBy(args: DropOwnedByArgs): Node;
  grant(args: GrantArgs): Node;
  revoke(args: RevokeArgs): Node;
  createPolicy(args: CreatePolicyArgs): Node;
  dropPolicy(args: DropPolicyArgs): Node;
  createFunction(args: CreateFunctionArgs): Node;
  dropFunction(args: DropFunctionArgs): Node;
  raw(args: PgRawArgs): Node;
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

export const pg: PgNamespace = {
  createSchema(args) {
    requireString(args.name, "pg.createSchema({ name })");
    return record({
      op: "createSchema",
      name: args.name,
      ifNotExists: args.ifNotExists,
      authorization: args.authorization,
    });
  },
  dropSchema(args) {
    requireString(args.name, "pg.dropSchema({ name })");
    return record({
      op: "dropSchema",
      name: args.name,
      ifExists: args.ifExists,
      cascade: args.cascade,
    });
  },
  createExtension(args) {
    requireString(args.name, "pg.createExtension({ name })");
    return record({
      op: "createExtension",
      name: args.name,
      ifNotExists: args.ifNotExists,
      schema: args.schema,
    });
  },
  dropExtension(args) {
    requireString(args.name, "pg.dropExtension({ name })");
    return record({ op: "dropExtension", name: args.name, ifExists: args.ifExists });
  },
  createRole(args) {
    requireString(args.name, "pg.createRole({ name })");
    return record({
      op: "createRole",
      name: args.name,
      login: args.login,
      password: args.password,
      bypassRls: args.bypassRls,
      createRole: args.createRole,
      createDb: args.createDb,
      superuser: args.superuser,
      inRole: args.inRole,
      setSearchPath: args.setSearchPath,
      ifNotExists: args.ifNotExists,
    });
  },
  alterRole(args) {
    requireString(args.name, "pg.alterRole({ name })");
    return record({
      op: "alterRole",
      name: args.name,
      setSearchPath: args.setSearchPath,
      resetSearchPath: args.resetSearchPath,
    });
  },
  dropRole(args) {
    requireString(args.name, "pg.dropRole({ name })");
    return record({ op: "dropRole", name: args.name, ifExists: args.ifExists });
  },
  dropOwnedBy(args) {
    if (!Array.isArray(args.roles)) {
      throw structuredError("OP_INVALID", "pg.dropOwnedBy({ roles }): roles must be an array");
    }
    return record({ op: "dropOwnedBy", roles: args.roles });
  },
  grant(args) {
    if (!Array.isArray(args.privileges) || args.privileges.length === 0) {
      throw structuredError("OP_INVALID", "pg.grant({ privileges }): privileges must be a non-empty array");
    }
    if (args.on === null || typeof args.on !== "object") {
      throw structuredError("OP_INVALID", "pg.grant({ on }): on must be a target object");
    }
    if (!Array.isArray(args.to) || args.to.length === 0) {
      throw structuredError("OP_INVALID", "pg.grant({ to }): to must be a non-empty array");
    }
    return record({
      op: "grant",
      privileges: args.privileges,
      on: args.on,
      to: args.to,
      withGrantOption: args.withGrantOption,
    });
  },
  revoke(args) {
    if (!Array.isArray(args.privileges) || args.privileges.length === 0) {
      throw structuredError("OP_INVALID", "pg.revoke({ privileges }): privileges must be a non-empty array");
    }
    if (args.on === null || typeof args.on !== "object") {
      throw structuredError("OP_INVALID", "pg.revoke({ on }): on must be a target object");
    }
    if (!Array.isArray(args.from) || args.from.length === 0) {
      throw structuredError("OP_INVALID", "pg.revoke({ from }): from must be a non-empty array");
    }
    return record({
      op: "revoke",
      privileges: args.privileges,
      on: args.on,
      from: args.from,
    });
  },
  createPolicy(args) {
    requireString(args.name, "pg.createPolicy({ name })");
    requireString(args.table, "pg.createPolicy({ table })");
    if (Array.isArray(args.to) && args.to.length === 0) {
      throw structuredError("OP_INVALID", "pg.createPolicy({ to }): to must be a non-empty role array (omit to for PUBLIC)");
    }
    if (args.using === undefined) {
      throw structuredError("OP_INVALID", "pg.createPolicy({ using }): using is required (the renderer always emits USING)");
    }
    return record({
      op: "createPolicy",
      name: args.name,
      table: args.table,
      schema: args.schema,
      forCmd: args.for || "all",
      to: args.to,
      using: __pgResolveExpr(args.using),
      withCheck: __pgResolveExpr(args.withCheck),
    });
  },
  dropPolicy(args) {
    requireString(args.name, "pg.dropPolicy({ name })");
    requireString(args.table, "pg.dropPolicy({ table })");
    return record({
      op: "dropPolicy",
      name: args.name,
      table: args.table,
      schema: args.schema,
      ifExists: args.ifExists,
    });
  },
  createFunction(args) {
    requireString(args.name, "pg.createFunction({ name })");
    requireString(args.returns, "pg.createFunction({ returns })");
    requireString(args.language, "pg.createFunction({ language })");
    requireString(args.body, "pg.createFunction({ body })");
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
  },
  dropFunction(args) {
    requireString(args.name, "pg.dropFunction({ name })");
    return record({
      op: "dropFunction",
      name: args.name,
      schema: args.schema,
      argTypes: args.argTypes,
      ifExists: args.ifExists,
    });
  },
  raw(args) {
    requireString(args.sql, "pg.raw({ sql })");
    requireString(args.reason, "pg.raw({ reason })");
    return record({
      op: "pgRaw",
      sql: args.sql,
      reason: args.reason,
    });
  },
};
