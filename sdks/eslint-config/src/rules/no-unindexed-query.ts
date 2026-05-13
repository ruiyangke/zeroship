/**
 * @zeroship/db v2 — Tier D1 ESLint rule.
 *
 * Flags `.find({...})`, `.findOne({...})`, and `.deleteMany({...})` calls
 * whose filter is a single-field equality on a column that is unlikely
 * to be indexed. The selectivity heuristic is intentionally simple for
 * V1 — false positives are acceptable (TODO: cross-reference the
 * declared schema to suppress warnings on `.index()`/`.unique()` keys).
 *
 * Shape modeled after [Convex's `no-missing-args`](https://docs.convex.dev/eslint).
 *
 * **Limitations (V1)**
 * - Heuristic: any single-field equality filter triggers a warning.
 *   We do not inspect the schema declaration to know whether that field
 *   is indexed. The runtime-side warning in `@zeroship/db`'s
 *   `Collection.find()` already does that check and dedupes; the lint
 *   rule is a static-analysis nudge that fires earlier (in the editor).
 * - We only inspect direct `obj.method({ key: lit })` patterns. Spread
 *   filters (`.find(filter)`), template literals, and dynamic keys are
 *   not flagged.
 *
 * Wiring this into a project:
 * ```js
 * // eslint.config.js
 * import zeroship from "@zeroship/eslint-config";
 * export default [zeroship.recommended];
 * ```
 * Or pick the rule individually:
 * ```js
 * import noUnindexedQuery from "@zeroship/eslint-config/rules/no-unindexed-query";
 * export default [{ rules: { "@zeroship/no-unindexed-query": ["warn"] }, plugins: { "@zeroship": { rules: { "no-unindexed-query": noUnindexedQuery } } } }];
 * ```
 */

/**
 * Minimal subset of the ESLint RuleModule shape — defined locally so the
 * package doesn't pull in `eslint` as a dependency. Consumers project
 * the export onto their `RuleModule` type at the call site.
 */
export interface RuleModule {
  meta: {
    type: "problem" | "suggestion" | "layout";
    docs: {
      description: string;
      recommended?: boolean;
      url?: string;
    };
    schema: unknown[];
    messages: Record<string, string>;
  };
  create(context: RuleContext): Visitor;
}

interface RuleContext {
  report(descriptor: {
    node: AstNode;
    messageId: string;
    data?: Record<string, string>;
  }): void;
}

interface AstNode {
  type: string;
  // Common subset used by this rule; ESLint passes full ESTree nodes.
  callee?: AstNode;
  arguments?: AstNode[];
  property?: AstNode;
  name?: string;
  value?: unknown;
  raw?: string;
  computed?: boolean;
  object?: AstNode;
  properties?: Array<{
    type: string;
    key?: AstNode;
    value?: AstNode;
    computed?: boolean;
    shorthand?: boolean;
  }>;
  loc?: unknown;
}

interface Visitor {
  CallExpression?(node: AstNode): void;
}

/**
 * Method names on a `Collection` that take a filter as their first arg
 * and are worth flagging for missing indexes. We deliberately skip
 * `updateOne` / `updateMany` / `findOneAndUpdate` because the proposal
 * scoped D1 to read-path queries (`.find` family) and bulk deletes.
 */
const FLAGGED_METHODS = new Set(["find", "findOne", "deleteMany"]);

/**
 * Property keys that we never warn on — they are auto-indexed by every
 * collection (PRIMARY KEY) or are sentinel filter operators rather than
 * column names.
 */
const ALWAYS_INDEXED_OR_OPERATOR = new Set([
  "id",
  "_id",
  "$and",
  "$or",
  "$not",
]);

/**
 * Extract the literal string key from an object-expression property.
 * Returns `undefined` when the key is computed (dynamic), a number, or
 * any non-string shape we can't statically reason about.
 */
function staticKeyName(prop: { key?: AstNode; computed?: boolean }): string | undefined {
  if (prop.computed === true) return undefined;
  const k = prop.key;
  if (!k) return undefined;
  if (k.type === "Identifier" && typeof k.name === "string") return k.name;
  if (k.type === "Literal" && typeof k.value === "string") return k.value;
  return undefined;
}

/**
 * Returns the list of static top-level keys in an ObjectExpression. Any
 * property we can't analyse (spread, computed, non-literal) makes us
 * return `undefined` — we don't want to warn on a filter we can't
 * understand statically.
 */
function staticFilterKeys(node: AstNode): string[] | undefined {
  if (node.type !== "ObjectExpression" || !node.properties) return undefined;
  const keys: string[] = [];
  for (const p of node.properties) {
    if (p.type !== "Property") return undefined;
    const name = staticKeyName(p);
    if (name === undefined) return undefined;
    keys.push(name);
  }
  return keys;
}

const rule: RuleModule = {
  meta: {
    type: "suggestion",
    docs: {
      description:
        "Flag @zeroship/db `.find/.findOne/.deleteMany` calls whose filter " +
        "appears to do a sequential scan (single-field equality without an " +
        "obvious index). Match the runtime warning emitted in dev mode.",
      recommended: true,
      url: "https://github.com/zeroship-dev/zeroship/blob/main/docs/proposals/zeroship-db-v2.md#d1-index-awareness",
    },
    schema: [],
    messages: {
      unindexedQuery:
        "Likely unindexed query: `.{{method}}({ {{keys}} })` filters on a " +
        "field that may not have an index. Add `.index()` or `.unique()` " +
        "to the matching field in your schema, or accept the sequential " +
        "scan and silence this rule.",
    },
  },
  create(context) {
    return {
      CallExpression(node) {
        // Match `obj.method(arg)` shape only.
        const callee = node.callee;
        if (!callee || callee.type !== "MemberExpression") return;
        if (callee.computed === true) return;
        const methodName = callee.property?.name;
        if (typeof methodName !== "string") return;
        if (!FLAGGED_METHODS.has(methodName)) return;

        const args = node.arguments ?? [];
        if (args.length === 0) return;
        const filter = args[0];
        const keys = staticFilterKeys(filter);
        if (!keys || keys.length === 0) return;

        // Filter out operator-only keys (e.g. `{ $or: [...] }` alone).
        const concreteKeys = keys.filter((k) => !k.startsWith("$"));
        if (concreteKeys.length === 0) return;

        // Skip if all keys are auto-indexed (e.g. just `id`).
        const flaggable = concreteKeys.filter(
          (k) => !ALWAYS_INDEXED_OR_OPERATOR.has(k),
        );
        if (flaggable.length === 0) return;

        context.report({
          node,
          messageId: "unindexedQuery",
          data: {
            method: methodName,
            keys: flaggable.join(", "),
          },
        });
      },
    };
  },
};

export default rule;
