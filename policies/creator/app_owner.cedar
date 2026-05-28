// App owner - universal access to this specific app.
permit (
  principal is User,
  action,
  resource is App
) when {
  principal has app_owner_of
  && resource in principal.app_owner_of
};
