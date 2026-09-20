// Band: developer and above (rank >= 20). App-, database- and project-scoped
// only; a developer holds no organization authority at all, which is the
// absence of an `Organization` statement in this file rather than a forbid
// somewhere else.
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

// The database half of the band. A database is PROJECT-owned, so its authority
// is the seat on its project and there is no per-database role and no app
// indirection: `zeroship_authz::authority::resolve` reaches
// zeroship.databases.project_id directly, never a binding. That is also why no
// database action appears in the App statement above - an app's binding is data
// access for running code, not authority over the database it reaches.
//
// `database:migrate` sits at developer rank beside `apps:deploy` because the
// two are the same boundary seen twice: deployed code already rewrites every
// row it can reach, so the authority to change the shape of those rows belongs
// in the same band. It is NOT `migrations:approve`, which is the operator-only
// go-live channel for a destructive op and appears in no band at all.
permit (
  principal is User,
  action in [Action::"database:write", Action::"database:migrate"],
  resource is Database
) when { context.effective_rank >= 20 };

// The same authority named at the project. `apps:write` here is "create an app
// in this project" and `database:write` is "create a database in this project"
// - neither exists yet, so there is no App and no Database to name.
permit (
  principal is User,
  action in [Action::"apps:write", Action::"apps:deploy", Action::"apps:archive",
             Action::"env:write", Action::"secrets:read", Action::"secrets:write",
             Action::"database:write", Action::"database:migrate"],
  resource is Project
) when { context.effective_rank >= 20 };
