import { table, t } from "@zeroship/migrate";

// Closing an organization, and why the close is a timestamp rather than a
// DELETE.
//
// `projects.organization_id` is ON DELETE RESTRICT, so a hard delete of an
// organization that still owns a project cannot even run. That is not the
// deciding argument, though: the organization is the BILLING SUBJECT, and every
// invoice, dispute and status transition names it. A row that vanished would
// take the counterparty out of a money record that has to outlive the
// relationship it describes.
//
// So `dissolved_at` is the close, and the control plane refuses the close while
// any project remains -- naming the remedy, because a refusal that does not is a
// dead end. Members, invitations and billing history survive; the row accepts no
// further change once the timestamp is set.
//
// ---------------------------------------------------------------------------
// A CLOSED ORGANIZATION RELEASES THE NAMES IT WAS HOLDING
// ---------------------------------------------------------------------------
//
// Both uniqueness rules become "unique among LIVE organizations", and this is
// the half of the change that is not obvious until it bites.
//
// `organizations_slug_key` was global, so a creator who closed "Acme" could
// never create another "Acme": the name was held by a row they can no longer
// see, edit or delete. The slug is a human-facing handle, not an identity - the
// identity is `id`, which nothing here touches - so holding it forever is a
// closed record laying claim to a live namespace.
//
// `organizations_personal_owner_key` is the same defect with teeth. A personal
// organization is where `zeroship deploy` lands on a fresh account, resolved
// through that pointer, and its slug is derived from the owner's user id so it can
// never be re-derived differently. Left global, closing one would answer the
// creator's next deploy with a refusal they could not clear by any route: the
// pointer would still resolve to the closed row, and the unique index would
// refuse a replacement. Both indexes therefore gain `dissolved_at IS NULL`.
//
// THE POINTER IS KEPT RATHER THAN CLEARED. Clearing it is the personal-to-
// shared conversion that `transfer_ownership` performs, and a dissolved
// organization was not converted to anything - it ended. The record stays
// truthful and the READ (`personal_organization_of`) carries the filter, which
// is one place rather than one per writer.
//
// THE `ON CONFLICT` INFERENCE CLAUSE MOVES WITH THE INDEX. PostgreSQL matches a
// partial unique index only when the statement's own `WHERE` implies the index
// predicate, so `ensure_personal_organization`'s
// `ON CONFLICT (personal_owner_id) WHERE ...` names both conjuncts. A mismatch
// is not a silent widening; it is "there is no unique or exclusion constraint
// matching the ON CONFLICT specification" on the first deploy of a fresh
// account.
//
// ---------------------------------------------------------------------------
// WHAT IS DELIBERATELY NOT HERE
// ---------------------------------------------------------------------------
//
// NO CHECK. "A dissolved organization owns no projects" is a claim about a set,
// so a single-row CHECK cannot carry it -- the same reason "an organization
// keeps at least one owner" is not one. It is enforced by the predicate in the
// UPDATE that sets this column, under the organization row lock that makes the
// count true at commit as well as at read.
//
// NO INDEX ON `dissolved_at` ITSELF. Nothing scans for closed organizations:
// every read of the column is by primary key, inside the `SELECT ... FOR UPDATE`
// the control plane already takes on the row it is about to change.
//
// NO COLLATION REGISTRATION. Every typed-id text column and every foreign-key
// copy of one needs bytewise ordering
// (db/migrations-ts/20260831000001_sortable_entity_id_collations.ts); a
// timestamp is neither.
export default {
  name: "organization_dissolution",
  schema() {
    table("organizations", { schema: "zeroship" })
      .column("dissolved_at")
      .add({ type: t.timestamp() });

    // The slug, freed for reuse once the organization holding it is closed.
    // Dropped as a CONSTRAINT because that is what it was created as; the new
    // one is an INDEX, because only an index can carry a predicate.
    table("organizations", { schema: "zeroship" })
      .constraint("organizations_slug_key")
      .drop();
    table("organizations", { schema: "zeroship" })
      .index("organizations_live_slug_key")
      .add({
        on: ["slug"],
        unique: true,
        where: (col) => col("dissolved_at").isNull(),
      });

    // The personal-organization slot, same rule. Already partial; it gains the
    // second conjunct.
    table("organizations", { schema: "zeroship" })
      .index("organizations_personal_owner_key")
      .drop();
    table("organizations", { schema: "zeroship" })
      .index("organizations_live_personal_owner_key")
      .add({
        on: ["personal_owner_id"],
        unique: true,
        where: (col) => col("personal_owner_id").isNotNull().and(col("dissolved_at").isNull()),
      });
  },
};
