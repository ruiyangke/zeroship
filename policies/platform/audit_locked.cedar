// Audit-locked apps deny all writes while investigation locks are active.
forbid (
  principal,
  action,
  resource is App
) when {
  resource has audit_locked
  && resource.audit_locked == true
  && (action == Action::"apps:write"
   || action == Action::"apps:deploy"
   || action == Action::"apps:delete"
   || action == Action::"env:write"
   || action == Action::"secrets:write"
   || action == Action::"billing:write"
   || action == Action::"team:write"
   || action == Action::"deployments:rollback")
};
