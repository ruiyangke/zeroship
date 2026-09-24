# Building this zeroship app (agent guide)

Guidance for AI coding agents (Claude Code, opencode, Codex) working in this
project.

This is a **zeroship** app. You build it locally, `pnpm build` produces
`dist/app.zship`, and `zeroship deploy` ships it to the platform, which hosts,
runs and scales it.

## Skills

Detailed contracts live as skills in `.claude/skills/` (Claude Code) and
`.opencode/skills/` (opencode); the two directories hold identical files. If
your agent does not load skills automatically, read the file directly.

| Skill | Load it when |
| --- | --- |
| `zeroship-app` | Starting work here. Project shape, server vs browser, the `env.*` primitives. |
| `zeroship-rpc` | Adding or changing a server function, or a call returns 401 once deployed. |
| `zeroship-data` | Changing the schema under `migrations/`, or writing `env.db` queries. |
| `zeroship-deploy` | Building, deploying, migrating, or a deployed app fails its first database call. |

## The rules that bite hardest

1. **`"use server";` must be the first statement** of a module holding server
   functions. Without it those functions are bundled into the browser, where
   `env.*` does not exist.
2. **Migrations are the schema source of truth.** Add a migration under
   `migrations/`; never hand-edit `generated/zeroship/`. The build regenerates
   it and a production build fails when it drifts.
3. **Every procedure needs an explicit `id`.** A production build refuses a
   procedure whose id was defaulted from its export name.
4. **RPC is authenticated by default and `pnpm dev` does not enforce it.** A
   procedure with no policy works locally and returns 401 for everyone once
   deployed. Public endpoints need both `auth: "anonymous"` and
   `publiclyAccessible: true`.
5. **Deploy does not run migrations.** Run `zeroship migrate` whenever the
   schema changes, or the app's first database call fails with
   `schema_not_migrated` — the table or column was never created. `migrate`
   targets the DATABASE, not an app, so it needs no prior deploy and the two
   commands run in either order.

## Commands

```bash
pnpm install
pnpm dev        # local runtime, project-local SQLite under .zeroship/
pnpm build      # -> dist/app.zship

zeroship login  # once, device flow
zeroship deploy
zeroship migrate
```
