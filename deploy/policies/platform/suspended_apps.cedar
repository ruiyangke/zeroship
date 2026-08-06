// Suspended apps deny all writes. Reads still allowed, e.g. to view billing.
forbid (
  principal,
  action,
  resource is App
) when {
  resource has suspended
  && resource.suspended == true
  && (action == Action::"apps:write"
   || action == Action::"apps:deploy"
   || action == Action::"apps:delete"
   || action == Action::"env:write"
   || action == Action::"secrets:write"
   || action == Action::"billing:write"
   || action == Action::"team:write"
   || action == Action::"deployments:rollback")
};
