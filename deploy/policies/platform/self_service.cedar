// Self-service creator baseline — every authenticated User, regardless of
// platform role (including the zero-privilege DEFAULT_PLATFORM_ROLE "none"),
// may manage their OWN account and operate the platform-level, self-scoped
// surfaces:
//
//   - apps:write   create a new app (the creator is bound as its owner)
//   - apps:read    list THEIR apps (the list handler filters to owned apps for
//                  non-admins; this grant only authorizes the platform-level
//                  list/create surface, never a concrete foreign App)
//   - account:read / account:write  view + revoke their own OAuth grants
//
// CRITICAL: this is scoped to `resource is Resource` — the synthetic
// `Resource::"*"` entity that backs `Resource::Any`. It does NOT match a
// concrete `App` resource, so it grants ZERO authority over any specific app
// (foreign or owned). Per-app authority still flows exclusively through the
// `app_members`-bound creator policies (app_owner / app_editor / app_viewer).
// The C1 cross-tenant DENY (an un-roled creator reading a non-member App) is
// therefore preserved: those requests carry a concrete `App` resource, which
// this policy cannot match.
permit (
  principal is User,
  action,
  resource is Resource
) when {
  action == Action::"apps:read"
   || action == Action::"apps:write"
   || action == Action::"account:read"
   || action == Action::"account:write"
};
