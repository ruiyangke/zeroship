// The money axis, and it is a SECOND integer rather than a point on the first.
//
// The ladder gives `billing` billing_rank 20 at rank 10, and `admin`
// billing_rank 10 at rank 30. So a bookkeeper reads apps and cannot deploy,
// while an admin reads the invoice and cannot change the payout account.
// Collapsing the two into one number cannot express either seat.
//
// `context.billing_rank` is ORGANIZATION-level on every request, including a
// project- or app-scoped one: there is no per-project invoice, so
// zeroship_authz::authority never narrows it and project_members carries no
// billing dimension to narrow it with.
//
// BOTH STATEMENTS ARE ORGANIZATION-SCOPED ON PURPOSE. The organization is the
// billing subject. There is no App or Project statement here, so no per-app
// money authority exists to be reasoned about - which also means every billing
// route must name the organization it is acting on, not an app.

permit (
  principal is User,
  action in [Action::"billing:read"],
  resource is Organization
) when { context.billing_rank >= 10 };

permit (
  principal is User,
  action in [Action::"billing:write"],
  resource is Organization
) when { context.billing_rank >= 20 };
