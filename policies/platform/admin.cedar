// Platform admin - universal allow.
// Audited on every privileged action via control.authz_decisions.
permit (
  principal is User,
  action,
  resource
) when {
  principal has platform_role
  && principal.platform_role == "admin"
};
