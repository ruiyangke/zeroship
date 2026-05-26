import type { Db, SchemaInput } from "./db-types.js";

type ZeroshipSchemaDefault = typeof import("zeroship-schema").default;
type ZeroshipSchemaShape = ZeroshipSchemaDefault extends { schema: infer S }
  ? S
  : ZeroshipSchemaDefault;
type ZeroshipEnvDb = ZeroshipSchemaShape extends Record<string, SchemaInput>
  ? Db<ZeroshipSchemaShape>
  : ZeroshipDb;

declare module "zeroship" {
  interface Env {
    db: ZeroshipEnvDb;
  }
}

export {};
