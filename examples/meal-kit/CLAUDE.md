# Working on Gather

Read the repository `AGENTS.md`, this example's `README.md`, and the linked
website plan before changing product behavior. Read [COPY.md](COPY.md) before
adding or changing customer-facing text.

This example is a monorepo: `apps/storefront`, `apps/backoffice`, and
`packages/shared`. The two apps SHARE ONE DATABASE. `zeroship.jsonc`,
`migrations/` and `generated/` live at the workspace root and are declared
once; `env.db` is the same data in both apps. There is no call between the
apps - add one and you have reintroduced a boundary the config already removed.

Every app must name the workspace config explicitly (`configPath` and `app`
for the plugin, `ZEROSHIP_CONFIG` and `DATABASE_URL` for the runtime it
spawns), because the config loader does no upward directory walk. Both come
from `zeroship.workspace.ts`. A new entry point that starts or builds an app
has to load it.

Use Tailwind CSS and the local shadcn/Base UI components. Do not import
`@zeroship/ui`. Change theme tokens in `packages/shared/src/styles.css`, and keep the
food-focused layout responsive and keyboard accessible. Use Lingui's React hooks
and macros for interface copy. Chinese interface and bundled sample translations belong in
`packages/shared/locales/zh/messages.po`, never as literals in components or seed source. Staff
author recipe translations in the database; approval snapshots both languages.
Mark shared interface and seed strings for extraction with Lingui's `/* i18n */`
annotation. Market is independent of language.

Keep all prices, stock checks, ownership checks and staff authorization on the
server. A shared database is not a shared authority: the storefront's
procedures scope every read and write to the signed-in customer, and the back
office's check staff grants. Never add a staff command to the storefront or an
unscoped customer read to either. Payment outcomes and shipping events are
explicit simulations. Do not collect card information, imply a live charge or
bypass Zeroship's payment authorization model. Plan controls and existing
orders have separate lifecycles.

Author schema through `@zeroship/migrate` migrations in the workspace
`migrations/` directory and regenerate the checked-in artifacts with
`pnpm migrate`. One schema serves both apps. Do not edit generated database
types or import framework-internal bootstrap code. Private exports must remain
private at the RPC policy and handler layers. Workflows receive server-selected
inputs and use journaled steps for effects.

Each app's configured entry is its own `src/server.ts`, found by the plugin
from that app's root. Expose browser imports through that app's `src/api.ts`
and declare access in `src/server/config.ts`. Customer procedures belong to the
storefront and staff procedures to the back office; server helpers both need
live in `packages/shared/src/server`.

Run catalog validation, type checking, domain tests, browser journeys and the
production build for changes to the purchase pipeline. Keep rejection controls
alongside successful cases. A failed database or browser prerequisite is a
failed verification, not a passing result. Do not commit unless the user
explicitly requests it.

Recipe content is persisted application data. Render `recipeText` directly; do
not pass editorial strings through Lingui. The PO catalogs own interface copy
and sample translations. Compile them as ES modules for the server-side sample
loader. Public catalog reads must never seed data or invent remaining capacity.

Checkout attempts own timed recipe and delivery reservations. Accepted prices
remain fixed during verification. Expiry releases capacity without inventing a
payment result; late success enters review and cannot release fulfillment. Retry
only a definitively declined payment, using a fresh server quote and explicit
customer confirmation. Keep provider settlement and reservation state distinct.

Browser tests own a temporary copy of the workspace, its database, migrations,
storage and runtimes. Preserve the working demo's data and `.env`. Use a CLI
built from the current workspace via `ZEROSHIP_BIN`; an older binary on PATH
does not verify the current native code.
