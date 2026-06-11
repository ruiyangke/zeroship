# Codex brief — private registry (Verdaccio) Phase 1: stand it up + publish the SDKs

## Goal
Implement Phase 1 of `docs/decisions/2026-05-26-sdk-distribution-private-registry.md`: a
**Verdaccio** private npm registry scoped to `@zeroship`, publish every `sdks/*` package to
it, and a runbook. **No sandbox/builder changes this phase** (that's Phase 2).

Worktree: you are run with `-C .worktrees/ui-design` (branch `builder/ui-design`).
**DO NOT commit / merge / push** — the pilot reviews and commits.

## Do
1. **Verdaccio service**
   - Add a `verdaccio` service to the multi-node `docker-compose` (find it via
     `docs/runbooks/docker-compose.md`), port 4873, persisted storage volume, a committed
     `config/verdaccio/config.yaml`: `@zeroship/*` scope hosted here, uplink to npmjs for
     everything else, anonymous `access`+`read` allowed, `publish`/`unpublish` restricted to
     an authenticated user. Healthcheck on `/-/ping`.
   - For local `zeroship serve` dev, document running Verdaccio directly (`pnpm dlx
     verdaccio --config config/verdaccio/config.yaml`) — no need to wire it into the Rust
     dev binary.
2. **Make every `sdks/*` package publishable**
   - For each of `sdks/{ui,db,rpc,auth,kv,storage,migrations,react,bootstrap,vite-plugin}`
     (whatever exists): ensure `publishConfig.registry` points at the registry URL (use a
     var/`.npmrc`, not a hardcode if cleaner), remove `"private": true` where present,
     confirm `name`, `version`, `exports`, `files`/`dist` are publish-correct, and the
     package builds (tsup). NOTE: `@zeroship/bootstrap` is framework-internal (see
     `sdks/bootstrap/README.md`) — only publish it if other SDKs depend on it at install
     time; otherwise leave it unpublished. Use judgment and report what you published.
   - Pre-launch, no semver ceremony: a fixed or simply-bumped version is fine.
3. **Publish pipeline**: `scripts/publish-sdks.sh` — build all SDKs in dependency order
   (root `pnpm build` already respects the graph), then publish the `sdks/*` packages to the
   registry (`pnpm -r --filter "./sdks/*" publish --no-git-checks --registry <url>` or
   per-package), tolerant of re-publish (bump or `--force`-equivalent). Add a root
   `package.json` script alias (e.g. `"publish:sdks"`).
4. **Runbook** `docs/runbooks/private-registry.md`: start Verdaccio (local + compose),
   create the publish user/token, run the publish script, verify, and how a consumer points
   `@zeroship` scope at it (`.npmrc`: `@zeroship:registry=http://localhost:4873`).

## Verify (network available — you are run with the bypass flag; NO OpenAI)
- Start Verdaccio locally with the committed config (background it).
- Run `scripts/publish-sdks.sh` → publishes succeed.
- `npm view @zeroship/ui version --registry http://localhost:4873` resolves; same for
  `@zeroship/db` and `@zeroship/rpc`.
- In a throwaway temp dir OUTSIDE the monorepo (e.g. `mktemp -d`), with
  `.npmrc` `@zeroship:registry=http://localhost:4873`, run `pnpm add @zeroship/ui` → it
  installs from Verdaccio (not the workspace) and `node -e "require.resolve('@zeroship/ui')"`
  (or an ESM import check) resolves. Capture the output.
- `docker compose config` is valid with the new service (don't need full cluster up, but if
  quick, `docker compose up -d verdaccio` + healthcheck + publish into it).
- Stop any Verdaccio you started (numeric pid).

## Report (stdout)
- The compose service + config path; which `sdks/*` packages were made publishable +
  published (and any deliberately skipped, e.g. bootstrap, with why); the publish script +
  root alias; the runbook path.
- All verification output (publish log, `npm view` results, the throwaway-dir install
  proof).
- Anything that needs Phase 2 (sandbox `.npmrc` + network reachability + Builder deps).

## Constraints
- Pre-launch, no back-compat. Break/rename freely.
- Do NOT modify the sandbox image or the Builder agent this phase (Phase 2).
- **DO NOT commit / merge / push.**
