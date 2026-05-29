// Platform billing - billing management plus self-service account management.
permit (
  principal is User,
  action,
  resource
) when {
  principal has platform_role
  && principal.platform_role == "billing"
  && (action == Action::"billing:read"
   || action == Action::"billing:write"
   || action == Action::"apps:read"
   || action == Action::"account:read"
   || action == Action::"account:write")
};
