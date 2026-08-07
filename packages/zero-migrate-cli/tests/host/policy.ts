/**
 * The charter host tests apply with: an author-owned project schema and no
 * injected table shape.
 *
 * Every grant here is default-deny in the knob registry, so a charter that omits
 * one owns nothing at all. In particular a charter with no `schema.cross_schema`
 * grant owns ZERO schemas, and `GuardConfig::schema_scope` collapses to
 * `Single("")` -- which permits no schema, not every schema. The project schema
 * is generated per test, so the charter has to be built around it rather than
 * shared as one constant string.
 *
 * @param projectSchema the schema the migration is confined to, named literally.
 *   A globbed include contributes no OWNED schema, so the pattern must be the
 *   exact name.
 */
export function noInjectPolicy(projectSchema: string): string {
  const scope = `{ include = [${JSON.stringify(projectSchema)}] }`;
  return `policy_version = 1

# Own the project schema: this grant IS the cross-schema confinement scope, so
# without it the guard denies the suite its own schema.
[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

# The suite authors CREATE TABLE inside the project schema.
[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

# Drops (index, column, table) and DML deletes are destructive; the knob
# defaults to "forbid". It is Global, so "all" is its only legal scope.
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}
