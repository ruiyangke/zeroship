permit (
  principal,
  action,
  resource
)
when {
  principal.app_owner_of.contains(resource)
};

permit (
  principal,
  action in [
    Action::"apps:read",
    Action::"apps:write",
    Action::"apps:deploy",
    Action::"deployments:read",
    Action::"deployments:rollback",
    Action::"env:read",
    Action::"env:write",
    Action::"secrets:read",
    Action::"secrets:write"
  ],
  resource
)
when {
  principal.app_editor_of.contains(resource)
};

permit (
  principal,
  action in [
    Action::"apps:read",
    Action::"deployments:read",
    Action::"env:read",
    Action::"team:read"
  ],
  resource
)
when {
  principal.app_viewer_of.contains(resource)
};
