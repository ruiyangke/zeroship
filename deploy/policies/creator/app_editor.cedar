// App editor - deploy and environment writes, without secrets or billing.
permit (
  principal is User,
  action,
  resource is App
) when {
  principal has app_editor_of
  && resource in principal.app_editor_of
  && (action == Action::"apps:read"
   || action == Action::"apps:write"
   || action == Action::"apps:deploy"
   || action == Action::"env:read"
   || action == Action::"env:write"
   || action == Action::"deployments:read")
};
