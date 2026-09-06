// Band: developer and above (rank >= 20). App- and project-scoped only; a
// developer holds no organization authority at all, which is the absence of an
// `Organization` statement in this file rather than a forbid somewhere else.
//
// `secrets:read` is NAMES ONLY (env_handlers::list_secrets calls
// list_secret_names), so the read/write split here is not a data boundary. The
// data boundary is `apps:deploy`: deployed code reads every secret VALUE at
// runtime, which is why deploy and secret writes sit in the same band and why
// the viewer band has neither.
//
// `migrations:approve` appears in NO band, here or anywhere. The
// operator-versus-creator separation for destructive migration go-live
// therefore survives as the absence of a permit: every band is an allow-list,
// so an owner at rank 40 cannot self-approve their own bundle's destructive
// migration any more than a viewer can.

permit (
  principal is User,
  action in [Action::"apps:write", Action::"apps:deploy", Action::"apps:archive",
             Action::"env:write", Action::"secrets:read", Action::"secrets:write"],
  resource is App
) when { context.effective_rank >= 20 };

// The same authority named at the project. `apps:write` here is "create an app
// in this project" - the app does not exist yet, so there is no App to name.
permit (
  principal is User,
  action in [Action::"apps:write", Action::"apps:deploy", Action::"apps:archive",
             Action::"env:write", Action::"secrets:read", Action::"secrets:write"],
  resource is Project
) when { context.effective_rank >= 20 };
