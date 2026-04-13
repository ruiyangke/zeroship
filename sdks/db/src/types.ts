export type PrimitiveTypeName = "string" | "number" | "boolean" | "date" | "json";
export type ArrayTypeDef = { type: "array"; items: PrimitiveTypeName };
export type TypeName = PrimitiveTypeName | "array";

export interface FieldDef {
  type: TypeName;
  items?: PrimitiveTypeName;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: unknown;
  min?: number;
  max?: number;
  enum?: unknown[];
  pattern?: RegExp;
}

export class TypeBuilder {
  _def: FieldDef;

  constructor(def: FieldDef) {
    this._def = { ...def };
  }

  required(): this {
    this._def.required = true;
    return this;
  }

  unique(): this {
    this._def.unique = true;
    return this;
  }

  index(): this {
    this._def.index = true;
    return this;
  }

  default(val: unknown): this {
    this._def.default = val;
    return this;
  }

  min(n: number): this {
    this._def.min = n;
    return this;
  }

  max(n: number): this {
    this._def.max = n;
    return this;
  }

  enum(...values: unknown[]): this {
    this._def.enum = values;
    return this;
  }

  pattern(re: RegExp): this {
    this._def.pattern = re;
    return this;
  }
}

export const t = {
  string(): TypeBuilder {
    return new TypeBuilder({ type: "string" });
  },
  number(): TypeBuilder {
    return new TypeBuilder({ type: "number" });
  },
  boolean(): TypeBuilder {
    return new TypeBuilder({ type: "boolean" });
  },
  date(): TypeBuilder {
    return new TypeBuilder({ type: "date" });
  },
  json(): TypeBuilder {
    return new TypeBuilder({ type: "json" });
  },
  array(items: TypeBuilder): TypeBuilder {
    const itemType = items._def.type as PrimitiveTypeName;
    return new TypeBuilder({ type: "array", items: itemType });
  },
};
