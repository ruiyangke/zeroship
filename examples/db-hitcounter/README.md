# db-hitcounter

This app inserts a hit for each request and reads it through `env.db`. Its
build emits the migration IR and the runtime descriptor used by deployment.

Run the example-owned acceptance suite from the workspace:

```sh
pnpm install --frozen-lockfile
pnpm --dir examples/db-hitcounter typecheck
pnpm --dir examples/db-hitcounter test
```

Vitest builds the SDKs, app, and Rust services. Its TypeScript fixture owns
PostgreSQL, Redpanda, and an identity issuer through testcontainers, and starts
the control plane, worker, gateway, migration service, and CDC relay on allocated
ports. It then walks the sequence a creator walks: it creates an organization, a
project and a database through the control plane, waits for the cluster
reconciler to make the cluster match, and applies the migrations through the real
CLI **with no app deployed and no binding in existence** - a migration is
authorized at the database's own project, not at an app. It then builds the app
against that database, watches the deploy be refused while the app holds no
binding, grants one, and deploys.

Assertions verify that the reconciler minted the database's migrator and
capability roles, that the binding role inherits one capability role without being
able to assume it while the shared worker login may assume the binding and inherit
nothing from it, that the deployed app's rows are reachable through that narrowing
and refused without it, and that reapplying the same migration is a no-op.

Chromium drives real app requests. The test compares metering with the physical
PostgreSQL rows, waits for projected billing to converge, and verifies its
database oracle rejects a deliberately deleted metered row.

Use the repository's Rust and Node toolchains, Docker, and Chromium. If needed,
install the browser with `pnpm --dir examples/db-hitcounter exec playwright install chromium`.
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` selects an explicit browser executable.
The fixture cleans up its processes, containers, and temporary app copy.
Logs and failure screenshots remain under `tests/.artifacts/`. Missing
infrastructure or migration failures fail the suite.
