permit (
  principal,
  action in [
    Action::"apps:read",
    Action::"deployments:read",
    Action::"env:read",
    Action::"billing:read",
    Action::"team:read",
    Action::"account:read"
  ],
  resource
)
when {
  principal.platform_role == "readonly"
};
