// The self-service baseline: what every authenticated User may do WITHOUT
// holding a seat in any organization.
//
// This is the ONLY policy matching `Resource::Any` (the synthetic
// `Resource::"*"` entity), and that makes it the only fence against the
// wrapper. `scopes_to_policy` always emits `Resource::Any`, which
// `lower_resources` renders as a bare `resource` matching EVERY resource type,
// so a bearer token narrows the ACTION and nothing else. A rank-free permit for
// an organization action landing in this file would therefore hand that action
// to every bearer carrying its scope, at every organization in the fleet.
//
// Each action here confers authority over NOTHING that already exists:
//
//   - organization:create  mints a new organization and seats the caller as its
//                          owner. It cannot touch an existing one.
//   - organization:read    on Resource::Any is "list the organizations I belong
//                          to". The handler resolves that list from
//                          organization_members; reading a CONCRETE organization
//                          is organization_read.cedar, which needs rank >= 10.
//   - apps:read            is the platform-level list surface, filtered by the
//                          handler to apps the caller can reach. Reading a
//                          concrete App needs a rank.
//   - account:read/write   the caller's own profile and OAuth grants.
//
// `apps:write` USED TO BE HERE AND IS DELETED. Creating an app is now
// project-scoped - an app must name the project that owns it - so app creation
// is gated on `apps:write` at `Resource::Project`, where the developer band
// requires rank >= 20. Leaving it here would have let any authenticated
// principal create an app with no organization behind it.
//
// It is scoped to `resource is Resource`, so it matches NO concrete App,
// Project or Organization. The cross-tenant read that this scoping prevents -
// an un-seated creator reading someone else's app - stays prevented, because
// those requests carry a concrete resource type this policy cannot match.
permit (
  principal is User,
  action in [Action::"apps:read", Action::"organization:read",
             Action::"organization:create", Action::"account:read",
             Action::"account:write"],
  resource is Resource
);
