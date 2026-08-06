// App viewer - read-only access to this specific app.
permit (
  principal is User,
  action,
  resource is App
) when {
  principal has app_viewer_of
  && resource in principal.app_viewer_of
  && (action == Action::"apps:read"
   || action == Action::"env:read"
   || action == Action::"secrets:read"
   || action == Action::"billing:read"
   || action == Action::"team:read"
   || action == Action::"deployments:read")
};
