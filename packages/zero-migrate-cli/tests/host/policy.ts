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
/**
 * A charter that additionally lets a migration CREATE and DROP a second schema.
 *
 * Kept separate from {@link noInjectPolicy} rather than folded into it. Every other
 * host arm runs under the narrow charter, and adding a vendor grant there would
 * loosen all of them at once to serve one test -- an exemption that grows until it
 * covers the thing it was protecting.
 *
 * Both schemas go in the `schema.cross_schema` scope: that grant IS the confinement
 * set, so a migration that creates `authored` while owning only `projectSchema` is
 * touching a schema it does not own and is denied before the create is reached.
 *
 * @param projectSchema the schema the suite is confined to.
 * @param authored the schema the migration itself creates and drops.
 */
export function createSchemaPolicy(projectSchema: string, authored: string): string {
  const scope = `{ include = [${JSON.stringify(projectSchema)}, ${JSON.stringify(authored)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

# The vendor grant this charter exists for: authoring CREATE SCHEMA at all.
[[grant]]
key = "schema.create_schema"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

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
