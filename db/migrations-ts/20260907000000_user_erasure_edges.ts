import { table } from "@zeroship/migrate";

// Every reference to `zeroship.users(id)`, made survivable by the database
// rather than by a list the reaper walks.
//
// ---------------------------------------------------------------------------
// WHY A LIST CANNOT BE THE MECHANISM
// ---------------------------------------------------------------------------
//
// A hand-maintained list of the references PostgreSQL "does not clear by itself"
// is a second mechanism doing the database's work, and it is wrong exactly when
// someone adds a reference without reading it. It also cannot express `SET NULL`
// on a NOT NULL column, which is not even a spelling those columns accept. The
// list is deleted with this migration, not replaced by a longer one. The edge is
// the declaration.
//
// ---------------------------------------------------------------------------
// THE RULE, AND WHY EACH EDGE GETS THE ACTION IT GETS
// ---------------------------------------------------------------------------
//
// An IDENTITY edge -- a row that exists only because the human exists -- is
// CASCADE. An ATTRIBUTION edge -- a row about something else that happens to
// record who acted -- is SET NULL, and its column must therefore be nullable.
//
//   identity_links      (provider, provider_subject) -> principal. This IS the
//                       human's external identity. It cannot outlive them, and
//                       leaving it would let a later sign-in through the same
//                       provider subject resolve to a deleted user. CASCADE.
//
//   principal_grants    (principal_id, grant_name) is the scope ceiling the CLI
//                       is issued against. A grant with no holder is authority
//                       nobody is accountable for. CASCADE.
//
//   device_grants       principal_id is the human who APPROVED a device code.
//                       SET NULL would be actively unsafe here: `status` stays
//                       'approved' while the principal goes NULL, which is the
//                       shape of a grant that has been authorised by nobody.
//                       CASCADE.
//
//   app_schema_applies  submitted_by attributes one creator migration. The
//                       migration record is the app's, and it has to outlive
//                       the person who submitted it -- which is why RESTRICT
//                       was wrong rather than strict: it made the record's
//                       durability the human's problem. NOT NULL is dropped and
//                       the edge becomes SET NULL.
//
//   oauth_clients       created_by attributes a client registration that
//                       outlives its registrar. SET NULL -- the same action the
//                       deleted list was performing by hand, now declared.
//
// ---------------------------------------------------------------------------
// WHAT IS DELIBERATELY NOT CHANGED
// ---------------------------------------------------------------------------
//
// `organizations.personal_owner_id` stays SET NULL. CASCADE was considered and
// rejected: `zeroship.invoices` carries NO foreign key to `organizations`, so
// the database cannot refuse a delete that would orphan the ledger, and a
// cascade from a users row would be a money record losing its subject with
// nothing to stop it. The organization is closed by its own verb
// (`dissolved_at`) and the erasure REQUEST refuses while the principal is the
// sole owner of a live one -- a refusal the person who asked can act on, ahead
// of the window rather than inside it.
//
// NO COLLATION REGISTRATION. These columns already carry their internal `usr`
// storage contract; this migration changes only constraints and nullability.
export default {
  name: "user_erasure_edges",
  schema() {
    // ---- identity edges: CASCADE ------------------------------------------
    table("identity_links", { schema: "zeroship" })
      .constraint("identity_links_principal_id_fkey")
      .drop();
    table("identity_links", { schema: "zeroship" })
      .foreignKey("identity_links_principal_id_fkey")
      .add({
        columns: ["principal_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
      });

    table("principal_grants", { schema: "zeroship" })
      .constraint("principal_grants_principal_id_fkey")
      .drop();
    table("principal_grants", { schema: "zeroship" })
      .foreignKey("principal_grants_principal_id_fkey")
      .add({
        columns: ["principal_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
      });

    table("device_grants", { schema: "zeroship" })
      .constraint("device_grants_principal_id_fkey")
      .drop();
    table("device_grants", { schema: "zeroship" })
      .foreignKey("device_grants_principal_id_fkey")
      .add({
        columns: ["principal_id"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
      });

    // ---- attribution edges: SET NULL --------------------------------------
    // The NOT NULL comes off FIRST: SET NULL on a NOT NULL column is accepted
    // at declaration time and raises only when a delete tries to use it, so
    // leaving the order to chance would ship an edge that reads correct and
    // fails at the one moment it is needed.
    table("app_schema_applies", { schema: "zeroship" })
      .column("submitted_by")
      .dropNotNull();
    table("app_schema_applies", { schema: "zeroship" })
      .constraint("app_schema_applies_submitted_by_fkey")
      .drop();
    table("app_schema_applies", { schema: "zeroship" })
      .foreignKey("app_schema_applies_submitted_by_fkey")
      .add({
        columns: ["submitted_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
      });

    table("oauth_clients", { schema: "zeroship" })
      .constraint("oauth_clients_created_by_fkey")
      .drop();
    table("oauth_clients", { schema: "zeroship" })
      .foreignKey("oauth_clients_created_by_fkey")
      .add({
        columns: ["created_by"],
        references: { table: "users", columns: ["id"], schema: "zeroship" },
        onDelete: "setNull",
      });
  },
};
