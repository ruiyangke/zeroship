// Platform support - read-only access plus self-service account management.
permit (
  principal is User,
  action,
  resource
) when {
  principal has platform_role
  && principal.platform_role == "support"
  && (action == Action::"apps:read"
   || action == Action::"env:read"
   || action == Action::"secrets:read"
   || action == Action::"billing:read"
   || action == Action::"team:read"
   || action == Action::"account:read"
   || action == Action::"account:write"
   || action == Action::"deployments:read")
};
