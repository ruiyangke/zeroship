// Platform readonly - universal read-only access.
permit (
  principal is User,
  action,
  resource
) when {
  principal has platform_role
  && principal.platform_role == "readonly"
  && (action == Action::"apps:read"
   || action == Action::"env:read"
   || action == Action::"secrets:read"
   || action == Action::"billing:read"
   || action == Action::"team:read"
   || action == Action::"account:read"
   || action == Action::"deployments:read")
};
