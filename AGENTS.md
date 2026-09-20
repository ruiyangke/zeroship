# zeroship — from zero to ship

An AI-native app platform spanning development framework to hosting, end to end. Agents build; the platform runs, meters and bills what they build.

This file is the agent landing page. It carries **principles**: the constraints you cannot derive by reading the tree, and the standards your work has to meet. Everything else is in the tree, `docs/` and `cargo xtask`; commit conventions are in `CONTRIBUTING.md`.

---

## Development status — pre-launch, no back-compat

**zeroship has never been published.** No production users, no tenants, no creator apps in the wild. Every API, wire format and schema is fair game to break.

That is a deliberate stance, and it is about *back-compat obligations only*. "We are pre-launch" is never a reason to defer security, correctness, or a design done properly. For anything a launch would freeze — credential and trust models, wire formats, schema shapes — the argument runs the other way: changing them afterwards means migrating live tenants across every deployment, so now is the cheapest time, not the most deferrable. Build the end state instead of two intermediate versions you throw away.

- **No `@deprecated` aliases.** Rename the symbol, delete the old name, one change.
- **No shims, no detect-and-warn paths, no "legacy mode".** If the new shape is right, the old one disappears in the same change.
- **No back-compat for wire formats, SDK contracts or V8 RPC.** Break the shape and update every producer, consumer, fixture and reference doc in the same patch.
- **No "scan the creator codebase" tooling, and no backfills for existing tables.** There are no creator codebases and no creator tables in production.
- **Wire-format versioning is code-evolution discipline, not user-compat.** Ciphertext flags and sentinel formats exist so dev and test databases re-decrypt across runtime versions, not so deployed apps can be left alone.

---

## How to work here

1. **Verify; never assert.** Run the check and report its output verbatim. "Should pass" is not a result. A filter that matched no tests still prints `test result: ok`; a test gated out by a feature is not reported as skipped, it is absent from the binary. Count what a file declares against what ran, and confirm the target rather than the variable — a feature can cfg-swap the DSN and ignore the one you set.
2. **Read before you reason.** No claim about code you have not opened.
3. **Re-measure; never trust a number in prose.** A figure in a document is a claim nothing re-runs.
4. **Cite the path and the function; quote the code.** A line number is not a citation; it drifts silently. Every cited path must resolve.
5. **No statistics in durable artifacts.** No byte counts, timings, percentages, test counts or tallies in docs, comments or commit messages. State the shape, name the instrument, and put anything load-bearing behind a gate or a re-runnable script.
6. **No tombstones.** When you remove something, remove it. Never annotate it — no "used to", "previously", "no longer", "was deleted".
7. **Every fix adds a regression test that fails before the fix.**
8. **Exercise behavior, not implementation text.** Tests assert on behavior, compiler contracts, parsed artifacts or structured metadata. Never add or port a check that searches source for an expected spelling, and retire scanners as their suites migrate.
9. **Retire what you replace** — harness, scanner, doc, scaffold — in the same change, and update every reference to it.
10. **Leave the tree working.** Never a red build, never a deleted failing test, never a fix by suppression (`as any`, `@ts-ignore`, an empty catch).
11. **If an invariant is in your way, stop and ask.**

---

## Key invariants

These do not change. If you are about to violate one, stop and ask.

- **Zero tokio in the stack.** Shipped I/O runs on compio/io_uring. No workspace member may declare `tokio` or `tokio-*`, except a member-owned `[dev-dependencies]` entry with its own version for a test oracle such as `tokio-postgres`; the root `[workspace.dependencies]` table must not declare them at all, because its entries inherit into every dependency kind. The accepted transitive path through `cyper`, `hyper` and `hyper-util` still compiles Tokio — the graph does not establish which runtime drives I/O — so adding another Tokio-dependent package has to be raised. Enforced by `cargo xtask test repository`; update its sets and this invariant together when the boundary moves.
- **V8 per thread**, one isolate per (app, live deploy) plus a bounded budget of pinned workflow isolates per app for deploy-pinned replay. The worker evicts by LRU, and isolates `enter`/`exit` so one thread serves many apps.
- **typed_id everywhere.** UUIDv7 + base36 + entity prefix (`usr_`, `app_`, `ses_`), defined in the `zeroship-id` crate.
- **Wire formats are explicit contracts.** `Manifest`, `RouteEntry`, `AppRecord`, the `.zship` layout and RPC envelopes change deliberately, with every producer, consumer, fixture and doc in the same patch.
- **Native primitives are the kernel.** The Rust surface is small and stable on purpose; anything creator code can achieve through `fetch` or composition belongs in an npm package instead. Default to the package. Creator imports cannot resolve host modules, and no framework subpath is published.
- **The gateway is dumb.** It does manifest dispatch, JWT, rate-limit, CHWBL routing and asset proxying, and forwards. All app logic runs in the worker.
- **Metering is infrastructure.** There is no `env.meter`: usage is measured server-side at the platform and primitive boundaries so app code can neither forge nor suppress it.
- **Privilege follows the process.** The worker runs creator code, so a privileged database function it can invoke is not a security boundary. Runtime writes belong in the app's schema under scoped, parameterized SQL; privileged schema change, replication ownership and key management belong to the migration service, the CDC relay and the control plane respectively. Schema binding is the tenant boundary and runtime code cannot create schema objects. Creator-supplied actors pass through `sanitize_app_actor`; reserved system claims leave authorization and are retained for audit; unmask audit writes survive rollback of the creator transaction.
- **Dependency boundaries are deliberate.** Each V8 binding is a separate crate from its engine: `zeroship-kv`, `zeroship-storage`, `zeroship-data-orm` and `zeroship-metering` carry no V8 or adapter dependency, and `zeroship-data-macros` carries no runtime or driver dependency. `zeroship-workflow-schema` is a leaf, so a service can install the journal without depending on the engine.
- **The migration service is PostgreSQL-only.** It applies pure DDL and refuses anything else, including the SQLite rebuild step. The engine is multi-dialect; this host is not.
- **The platform schema has one migration source and one authoring package.** Creator and platform migrations both import the single `@zeroship/migrate`. A second implementation records into another ambient singleton and lets the host drain empty, so there is no alias and no second SDK package.
- **The edge is the authority on claimed names.** `deploy/ops/Caddyfile` declares the host blocks, and a name there is one a creator app may not register. Control's reserved-name list is pinned to that file's sha256, so editing the edge without regenerating fails `cargo test -p zeroship-control` rather than silently describing an edge you replaced. The gateway's public hostname is also its `iss` claim, so repointing it moves session issuance, not only a route.
- **Generated build inputs outlive their sources.** The napi addon, the V8 adapter bundle and the `dist` trees other build steps read are gitignored, so a checkout can hold one built from sources that have since moved. A stale one does not error. It answers from what its sources used to say, and the failure it causes names the consumer rather than itself; a stale compiler reports a correct artifact as stale, and "regenerating" overwrites the right answer with the old one. A generator's own `--check` cannot see this, because the generator is the thing that went stale. `cargo xtask test repository` compares each generated input against the files it is built from and names the command that rebuilds it; `pnpm build` runs the whole ordered chain. An absent artifact is a different condition and is left to the build, which fails on it by name.

---

## Build

Build the JavaScript packages before Cargo: the V8 database adapter embeds a generated artifact. `docs/runbooks/local-dev.md` has the full setup.

---

## Tests

- **Database verification is required.** It runs in ordinary `cargo test`, and is never opt-in, never ignored, and never reported as success when the server or a required extension is unavailable. `cargo xtask test <area>` builds whatever fixture a package needs and runs it; a fixture that cannot reach its server fails loudly and names what is missing.
- **Contracts live in their owning source crates**, grouped by backend and behavior, with private fixtures owning setup and teardown. Data fixtures own their own containers.
- **Keep nonempty-input assertions and rejection controls beside each check.** A check that can pass over zero input is not a check.
