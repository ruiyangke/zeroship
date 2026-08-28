import { table } from "zero-migrate";

// The platform staff role model is gone. `admin` was a literal universal allow
// - permit(principal is User, action, resource) - so one missed guard call was
// total cross-tenant compromise, and `support`/`billing`/`readonly` were narrow
// in ACTION but unconstrained in RESOURCE, which is the same property at a
// smaller radius: a `readonly` holder read every tenant's app metadata, env-var
// names and billing across the fleet.
//
// A hosting business does need a staff permission model. That model belongs to
// the vendor's own portal, against its own copy of the data, not as a
// cross-tenant grant in the policy set every deployment ships.
//
// The four `.cedar` files, the `platform_role` principal attribute and the
// three role-management routes are deleted in the same change, so nothing reads
// or writes this table afterwards.
export default {
  name: "drop_platform_admin_roles",
  schema() {
    table("platform_admin_roles", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
