# zeroship

From **zero** to **ship**. An AI-native framework, runtime and platform in one,
covering the whole application lifecycle — build, deploy and operate.

Build your app locally and put it online. The database, sign-in, file storage,
background jobs and payments are already part of the platform, so you start
from a working app instead of an empty server. It is built for creators —
developers and the coding agents working beside them — who would rather build
their product than run its infrastructure.

zeroship is pre-launch. Nothing here is publicly available, and the wire
formats, SDKs and schemas are still changing deliberately.

## What an AI-built app still needs

Writing the screens and the endpoints is the visible half of an app. The half
that decides whether it survives real users is everything around them:

- **Security** — who may call what, what the app may reach, and how secrets are
  held.
- **Auditing and logging** — what happened, who did it, and how you find out
  when something goes wrong.
- **Data** — a database with migrations, and row-level rules so one tenant can
  never read another's rows.
- **State** — key-value for sessions, counters and leases; object storage for
  files.
- **Delivery** — builds, deploys and schema changes that ship together instead
  of drifting apart.
- **Availability** — something that keeps serving when a request dies, a deploy
  lands, or a machine goes away.

## What zeroship gives you

Those are platform primitives here, not a checklist you assemble:

- **Auth** — platform-managed sign-in. Your app receives a per-app user
  identity and never handles a password or social credential.
- **Database** — managed, with committed migrations, a typed query surface, and
  row-level security policies declared in the schema.
- **Key-value** — per-key atomic, strongly consistent state for sessions,
  counters, leases and cache-aside values.
- **Object storage** — private per-app buckets.
- **Security** — each app runs in its own isolate with a per-app egress
  allow-list; secret values never live in the project, only their names.
- **Auditing** — privileged reads leave an audit record that survives a
  rollback of the caller's transaction.
- **Logging** — app output and request failures are recorded by the platform
  and readable per app.
- **Compliance-ready at day one** — the controls SOC 2, HIPAA and GDPR reviews
  ask for — row-level security, audit records, egress allow-lists, secret
  management and per-app isolation — are platform primitives, not a retrofit.
- **CDN** — static assets are served pre-compressed by the platform edge.
- **Availability** — the platform owns routing and isolation.
- **Durable workflows** — long-running tasks are first-class: steps, sleeps,
  signals and schedules whose progress is journaled, so a run survives
  restarts and redeploys.
- **RPC** — server functions are published as typed procedures with
  per-resource policy.
- **Metering, billing and payments** — usage is measured server-side; you set a
  plan, read back usage and charge, and your app can charge its own users.
- **Integrations** — connect the world with ease: sign-in providers, payments,
  and the third-party services your app depends on — integrations are a
  platform concern, not a per-app project.
- **Agents** — deploy and app management exposed to AI agents.

## How it works

You describe the app. Your agent builds it. zeroship runs it.

1. **Your agent builds.** It writes the app against the reference contract —
   screens, server functions, schema and policy — and checks itself with the
   local dev loop.
2. **One build packages it.** Code, routing and the database schema become a
   single deploy artifact, so they always ship together.
3. **You deploy.** The artifact goes up as-is, and schema changes are applied
   as their own step — a deploy that is ahead of its schema is refused rather
   than served.
4. **The platform operates it.** Requests are routed, assets served, code runs
   in isolation, usage is metered, and durable work survives restarts and
   redeploys.

## Start building

Point your coding agent at zeroship and ask it to build your app. The platform
is designed to be built by agents: the docs, the scaffold and the CLI are all
agent-readable, and they carry the defaults, limits and errors a model needs to
go from an idea to a running deploy without guessing.

- [`docs/reference/`](./docs/reference/) — the creator contract; give this to
  your agent first.
- [`AGENTS.md`](./AGENTS.md) — the principles behind the platform, for agents
  working on zeroship itself.