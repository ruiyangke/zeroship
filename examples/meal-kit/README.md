# Gather meal kits

A meal-kit monorepo with independently deployable customer and staff apps.
Both use React, Tailwind CSS, locally owned shadcn/Base UI components, Lingui,
React Router and Motion. The [website plan](../../docs/proposals/2026-09-11-meal-kit-example.md)
defines the wider product; [IMPLEMENTATION.md](IMPLEMENTATION.md) records gaps.

```text
meal-kit/
  zeroship.jsonc  ONE database and TWO apps declared against it
  migrations/     The shared schema, owned by the database
  generated/      env.db types folded from those migrations
  apps/
    storefront/   Customer website and its customer procedures
    backoffice/   Staff workspace and its staff procedures
  packages/
    shared/       Components, theme, translations, domain and server helpers
  scripts/        Setup, migrate and development commands for the pair
  tests/          Browser journeys and the isolated workspace fixture
```

The storefront owns discovery, country selection, the box wizard, checkout,
customer accounts and cooking. The back office owns recipe approval, country
menus, inventory, fulfillment, support, feedback and team access.

The two apps share one database. `zeroship.jsonc` declares `databases.main`
once and names it the `primary` of both apps, so `env.db` is the same data on
either side: a box the storefront writes is on the operations board when the
transaction commits, and a menu the back office publishes is what the storefront
sells. There is no call between the apps and nothing to synchronize.

One database means one writer at a time. Each app runs its own runtime process,
and in the dev tier both open the same SQLite file, where SQLite admits many
readers alongside a single writer. So the two apps do not write concurrently -
they take turns, bounded by the same lock budget a PostgreSQL deployment spends
on `lock_timeout`, and an explicit transaction takes the write lock on every
database the connection has open. At this example's volume that is invisible.
It is worth knowing before copying the shape into something write-heavy, where
the answer is PostgreSQL rather than a second database: two databases would put
the boxes and the board back on opposite sides of a call, which is the thing
this example exists to remove.

Each app still authorizes its own callers. The storefront's procedures scope
every read and write to the signed-in customer; the back office's check staff
grants. Staff sign in directly to the back office, on its own origin, so
browser auth cookies are never shared between them.

Prices, availability, ownership and staff permissions are checked on the
server. Payments, renewals, fulfillment and refunds are explicit simulations.
The example does not collect payment card details or make live charges.

## Run locally

Build the workspace prerequisites in the
[local development runbook](../../docs/runbooks/local-dev.md). Keep the CLI,
SDKs, migration addon and Vite plugin from the same checkout. From the repository
root:

```sh
pnpm install
cargo build -p zeroship-cli
export ZEROSHIP_BIN="$PWD/target/debug/zeroship"
cd examples/meal-kit
pnpm setup
pnpm dev
```

Open the storefront at <http://127.0.0.1:5197> and the back office at
<http://localhost:5199>. Use those hostnames to keep development auth cookies
separate; cookies are not isolated by port.

Setup writes each app's ignored `.env`, keeps existing configuration, and
applies the shared schema with `pnpm migrate`. Restarting preserves local data.
The migrations and the generated collection types live beside `zeroship.jsonc`
at the workspace root, under the database that owns them.

The config loader does no upward directory walk, so each app names the
workspace file explicitly: `zeroship({ configPath, app })` in its
`vite.config.ts`, and `ZEROSHIP_CONFIG` plus `DATABASE_URL` on the runtime the
dev server spawns. Both come from `zeroship.workspace.ts`, which every way of
starting an app loads. Skip it and the app gets a private, empty database.

Sign in as `alex@gather.example` or `sam@gather.example` in the storefront.
Sign in as `ops@gather.example` in the back office. On an empty database, open
Preview tools and load sample menus for each country you want to explore.
Sample loading is an explicit administrator action; browsing never creates
stock or publishes menus.

## Explore the apps

- Browse recipes, check local delivery, build a box and review checkout in the
  storefront. Language and delivery country are independent. China uses
  province, city and district fields; postal fields appear where applicable.
- Open the back office to find the customer's order, progress fulfillment and
  resolve a reported issue. Customer pages show the updated order state.
- Publish dated country menus from approved bilingual recipe versions. Drafts
  stay private, and purchased recipes retain their accepted versions and prices.
- Use Team access to assign country managers, menu editors, fulfillment staff
  and support staff. Shared recipe editing is a separate grant. Managers can
  refund within their countries; support staff can resolve requests without a
  refund. Deployment administrators manage grants and their audit history.
- Manage saved addresses, preferences, favorites, plans and privacy requests in
  the storefront. Cook from an order's recipe snapshot, change display units,
  use step timers and leave feedback after delivery.

Paused plans and existing orders have separate lifecycles. Renewals require
explicit confirmation in this demo. Deletion requests are queued for review;
submitting a request does not claim to delete retained commercial records.

## Configuration and deployment

One `zeroship.jsonc` declares both apps and the database they share. Each app
has its own server entry, client bundle and deploy artifact. Configure these
server values for the intended environment:

| Setting | App | Purpose |
| --- | --- | --- |
| `GATHER_ADMIN_IDS` | Both | Exact authenticated subjects that administer team access |
| `GATHER_MODE` | Both | Commerce mode; unconfigured live integrations fail closed |

Local `.env` names use the `ZS_VAR_` prefix so Zeroship exposes the values to
`env`. Both apps read the staff table to answer `getSession`, so both need the
administrator list.

Build the pair with `pnpm build`, then deploy each app from its own directory
using `pnpm deploy` and the [deployment golden path](../../docs/build-and-deploy-golden-path.md).
Apply the shared migrations once, from the workspace root. Production
acceptance must exercise the deployed gateway and PostgreSQL; local fixtures do
not establish that result.

## Verification and editing

```sh
pnpm typecheck
pnpm test
pnpm test:browser
pnpm build
```

Browser tests copy the workspace into a temporary directory and start the pair
there, with their own database, migrations and storage. They use separate
origins for staff and customers and exercise both apps through the real
development runtime. No spec starts until both apps have answered a procedure:
an open port is only Vite, and the runtime behind it takes longer to serve
because the pair contend for one workflow activation on the database they
share. Docker and a migrated PostgreSQL platform remain prerequisites for
deployed acceptance.

Shared UI primitives live in `packages/shared/src/components/ui`. They were
created with the shadcn CLI using the Base UI preset. Run
`pnpm exec shadcn add <component>` from an app directory; its UI alias resolves
to the shared component directory. Use `--dry-run` to review the destination.
App compositions handle
labels, validation and country rules. Keep translations in the shared Lingui
catalogs and use [COPY.md](COPY.md) when changing customer copy. The
[control review](UX-REVIEW.md) explains the wizard and navigation decisions.

Read [CLAUDE.md](CLAUDE.md) before editing. Preserve concurrent platform and
workflow work.
