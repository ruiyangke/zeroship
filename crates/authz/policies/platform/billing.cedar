permit (
  principal,
  action in [
    Action::"billing:read",
    Action::"billing:write",
    Action::"apps:read"
  ],
  resource
)
when {
  principal.platform_role == "billing"
};
