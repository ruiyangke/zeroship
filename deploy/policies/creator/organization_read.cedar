// Band: viewer and above (rank >= 10).
//
// HOW THE RANK GETS HERE. `context.effective_rank` is resolved server-side by
// zeroship_authz::authority::resolve, per request, from
// zeroship.organization_members joined to zeroship.organization_roles - and for
// a Project- or App-typed request, already NARROWED to
// min(organization rank, project rank) unless the caller is admin or above.
// It is never supplied by a caller: AuthzContext carries no field for it.
// A principal with no live membership carries 0.
//
// WHY CONTEXT AND NOT A PRINCIPAL ATTRIBUTE. eval::build_context supplies every
// key it declares on every request, so a missing membership fails CLOSED at the
// comparison below and the band is recorded as an ordinary non-match. A missing
// ENTITY attribute would instead make Cedar SKIP this policy with an evaluation
// error that matched_policy_ids never records - a deny that reads exactly like
// a clean no-match in the one durable record of the decision.
//
// EVERY STATEMENT DISCRIMINATES THE RESOURCE TYPE IN ITS SCOPE, never in its
// body. That is what stops an organization-scoped action being satisfied by an
// App-typed probe in is_authorized_anywhere, and it is why the consent screen's
// answer is the true one.
//
// An App statement and a Project statement carry the same actions on purpose:
// a route names a concrete app, while the consent probe and the "act inside
// this project" surfaces name the project. The project is the app's only path
// to an organization, so the two are the same authority seen from two heights.

permit (
  principal is User,
  action in [Action::"apps:read", Action::"env:read", Action::"deployments:read"],
  resource is App
) when { context.effective_rank >= 10 };

permit (
  principal is User,
  action in [Action::"apps:read", Action::"env:read", Action::"deployments:read",
             Action::"project:read", Action::"project:members:read"],
  resource is Project
) when { context.effective_rank >= 10 };

// Organization-scoped reads. `project:read` is deliberately NOT here: a member
// below admin holds no authority on a project they have no row for, so listing
// every project of the organization would hand them the names of projects they
// cannot reach. The "list projects I can see" surface is gated on
// `organization:read` and filtered by the handler.
permit (
  principal is User,
  action in [Action::"organization:read", Action::"organization:members:read"],
  resource is Organization
) when { context.effective_rank >= 10 };
