# Gather implementation ledger

The [website plan](../../docs/proposals/2026-09-11-meal-kit-example.md) is the
scope authority. This ledger links implemented behavior to its verification and
keeps unfinished work visible. Demo evidence does not establish live readiness.

| Area             | Implemented                                                                                                                                         | Remaining work                                                                                       |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Storefront       | Public browsing, market entry, recipe detail, search, favorites, eligibility, waitlist, mobile navigation and reduced-motion-aware page transitions | Public server rendering and search metadata                                                          |
| Customer copy    | Reviewed English and Chinese journeys, country-aware form labels, focused validation errors, shared preview notice and staff-only payment scenarios | Apply the [copy guidance](COPY.md) to new functionality and review it in the rendered journey        |
| Localization     | Lingui catalogs, independent language and market routing, formatting                                                                                | Expanded and RTL layout coverage, regional content and access checks                                 |
| Box and checkout | Delivery and box wizard, availability calendar, box review, signed server drafts, account attachment, conflict resolution, destination-aware quotes, expiring holds, payment recovery | Scheduled expiry dispatch and draft retention, promotions and add-ons, adjustment lifecycle |
| Account          | URL-backed account tabs, box edits, market-scoped plans, reviewed renewals, address book, preferences, privacy requests and exports                 | Payment methods, deletion processing and effective-dated plan settings                               |
| Recurrence       | Explicit demo renewal                                                                                                                               | Persisted cycles, automatic dispatch, consent exceptions, reminders, cutoff and recovery             |
| Catalog          | Persisted bilingual recipe drafts, quantities, immutable approval versions, dated market menus, sale windows, local prices and publication history  | Reusable ingredient library, media uploads and reusable price books          |
| Cooking          | Purchased recipe snapshots with fixed packed portions, unit controls, ingredient and step checklists, equipment and storage guidance, print view, timers retained across navigation and owned recipe feedback | Reviewed live recipe and nutrition content |
| Support          | Owned cases, staff resolution and bounded demo refunds                                                                                              | Private evidence, affected lines, credits, replacements, progress history                            |
| Fulfillment      | Capacity, fulfillment transitions, packing CSV                                                                                                      | Suppliers, receiving, lots, allocation, procurement, quality checks, shipments and recall            |
| Operations       | Audited team grants, country roles, separate shared recipe editing and server-enforced action permissions                                                                                                                   | Site scopes, customer tools, reports, business configuration                              |
| Integrations     | Explicit commerce simulation, private workflow export                                                                                               | Shared inbox/outbox, retries, reconciliation, provider adapters and scheduled verification           |
| Payments         | Server-authoritative demo amounts                                                                                                                   | App-scoped platform authorization, lifecycle operations, seller configuration and sandbox evidence   |
| Markets          | Published country-specific menus, postal or administrative service areas, address forms, currencies, local calendars and cutoff policy snapshots    | Persisted market/site configuration, seller and tax setup, reviewed live service zones               |
| Handoff          | Example-local app monorepo, shared UI and domain package, one database shared by both apps, local setup, gallery entry, generated schema, isolated app fixtures, browser and unit checks                                                        | Deployed PostgreSQL/gateway acceptance and reproducible provisioning                                 |
| Growth           | Saved favorite recipes and order-linked feedback reviewed by market                                                                                  | Planned referrals, gifts, loyalty, corporate ordering and recommendations after commerce foundations |

## Verification

Run the commands in [README.md](README.md). Behavioral checks live under
the apps' `tests/` directories and the monorepo's `tests/browser/`; schema authority lives under
`apps/backoffice/migrations/`. Record actual acceptance
in runnable checks rather than progress percentages. Keep platform work confined
to dependencies that the app requires and preserve the concurrent workflow work.

## Live operation dependencies

Enabling a live market requires its seller identity, provider sandbox and live
credentials, reviewed recipes, pricing, delivery partner and published policies.
Implement configuration and provider contracts before requesting these inputs.
Unconfigured commerce must fail closed; a demo result must remain recognizable.
