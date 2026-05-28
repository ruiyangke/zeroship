// Platform billing - billing management plus read-only app access.
permit (
  principal is User,
  action,
  resource
) when {
  principal has platform_role
  && principal.platform_role == "billing"
  && (action == Action::"billing:read"
   || action == Action::"billing:write"
   || action == Action::"apps:read")
};
