// App owner - universal access to this specific app.
//
// PR9c: `migrations:approve` (the operator-only go-live approval channel that
// COMPLETES an online-rename EXPAND / a scoped destructive migration) is
// EXCLUDED from the owner's otherwise-universal app authority. The owner is the
// bundle AUTHOR — the very principal the migration-approval anti-bypass must
// exclude — so the operator-vs-creator separation requires that owning an app
// does NOT grant the authority to self-approve that app's destructive
// migrations. Only the platform `admin` role (admin.cedar, universal allow)
// grants `migrations:approve`.
permit (
  principal is User,
  action,
  resource is App
) when {
  principal has app_owner_of
  && resource in principal.app_owner_of
  && action != Action::"migrations:approve"
};
