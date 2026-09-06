// Band: admin and above (rank >= 30).
//
// This is the rank at which per-project narrowing stops applying: the resolve
// returns the organization rank unchanged for EVERY project in the
// organization, so an admin reaches a project with no project_members row while
// a developer with the identical organization seat reaches nothing. Both facts
// come out of one function, zeroship_authz::authority::effective_project_rank,
// and the threshold is read from the ladder rather than written here - move
// `admin`'s rank in a migration and this band moves with it.
//
// Seating members is ORGANIZATION-scoped, so there is no App statement in this
// file. `organization:admin` - seating owners and admins - is deliberately NOT
// here: it belongs to the owner band alone.

permit (
  principal is User,
  action in [Action::"organization:members:write", Action::"project:create"],
  resource is Organization
) when { context.effective_rank >= 30 };

// Project administration. A project seat cannot reach this on its own: the
// effective rank is min(organization rank, project rank), so a developer seated
// as project owner resolves to 20 and is refused. A project grants and
// ceilings; it never widens.
permit (
  principal is User,
  action in [Action::"project:write", Action::"project:members:write"],
  resource is Project
) when { context.effective_rank >= 30 };
