# Secure by default, including local runs

- **Status:** proposal, not implemented
- **Date:** 2026-08-12
- **Scope:** the five server binaries (control, gateway, worker, auth, migrated)
  and the `zeroship` CLI

## Relationship to existing work

This proposal takes the naming and supply-set spine as given and adds the
security-posture dimension. It does not restate them.

- `docs/decisions/2026-05-28-server-config-unification.md` (Accepted) fixes the
  mechanism: `clap` + `serde` + `toml`, precedence `CLI flag > env > file >
  compiled default`, secrets never in the TOML, `--check-config` on every
  binary. `config-rs`, `figment` and `confique` were evaluated and rejected.
  None of that is reopened here.
- `docs/proposals/2026-08-11-config-name-alignment.md` proposes one canonical
  name per value with mechanically derived spellings, and a type-driven supply
  set (`Operational<T>`, `Secret<T>`, `DevEnv<T>`, `TestEnv<T>`, ...). This
  proposal assumes that lands. It adds the part that proposal does not cover:
  `--dev-insecure` and how a local run satisfies a security check instead of
  skipping it.

## The problem, measured

`--dev-insecure` is one flag standing for at least six unrelated concerns.
Counted 2026-08-12 with `grep -rn ... crates/`: 47 non-test occurrences of
`dev_insecure` (71 including tests) and 391 non-test occurrences of the internal
`insecure_dev` (456 including tests), across 82 files, 38 of them in `control`.

| what it does | when it applies |
| --- | --- |
| bypasses secret-strength validation | startup |
| allows boot without MASTER_KEY / CONTROL_KEY / --signing-key-file | startup |
| generates a random PAT signing key per boot | startup |
| selects `http` instead of `https` for cookie and redirect URLs | request time |
| relaxes the same-origin guard | request time |
| skips Stripe webhook signature verification | request time |

The last is not inferred. `crates/control/src/stripe_handlers.rs:1081`: when the
webhook secret is empty, `insecure_dev` skips verification rather than returning
500.

**The flag is running on the internet-facing deployment.** `zeroship.co` runs
control, gateway, auth and migrated with `--dev-insecure`, and
`deploy/compose/docker-compose.yml` carries a "DEV ONLY - NOT for production"
banner while being the only deployment that exists. It is there for exactly one
legitimate reason: Cloudflare terminates TLS, so the origin genuinely speaks
`http` and needs `scheme = http`. The other five behaviours came along because
the flag is all-or-nothing.

That is the whole case. "Dev only" is a claim, not a mechanism. Nothing
prevented it and nothing noticed.

A second cost is testing. If local runs take the relaxed path, the SECURED path
is exercised only in production, which is the one place it must not be wrong.

## The principle

This repo already solved this once, for auth. From
`docs/reference/auth-dev-tier.md`, "Dev-only by construction":

> It lives in `@zeroship/bootstrap`'s `dev-auth.ts`, imported ONLY by
> `dev-entry.ts`. The production `runtime-entry.ts` never imports it. It is
> deliberately NOT re-exported from the barrel ... a barrel re-export would risk
> pulling the provider into the shipped worker.

The dev affordance cannot reach production because of module topology, not
because a flag says so. `--dev-insecure` is the inverse: one binary, one code
path, a runtime boolean.

> **A dev affordance must be unreachable in production by construction. If it is
> reachable and merely disabled, it will eventually be enabled.**

## Three buckets

Sorting the six behaviours by what they actually are. Only one bucket is about
security.

### A. Not a security relaxation at all

`origin-scheme = http` behind a TLS-terminating proxy is CORRECT. So is naming
the origins the same-origin guard should trust. These become ordinary
`Operational<T>` settings with ordinary names:

```
origin_scheme      = "http" | "https"
trusted_origins    = [ ... ]
```

They carry no "insecure" in the name because they are not insecure. This bucket
is the reason the flag is on in production, and it is the cheapest to fix.

### B. Checks that always run

Signature verification, secret-strength validation, the same-origin guard. Never
conditional, no environment can turn them off. What varies is where their INPUTS
come from, not whether they execute.

### C. Provisioning those inputs, which moves OUT of the server

Today the asymmetry lives inside the server: `if !insecure_dev { require
secrets }`. Instead the server always requires real inputs, and a separate step
produces them for a local run:

```
zeroship dev init      # generate a strong secrets dir + dev overlay, gitignored
```

The server binary is then identical in dev and production: it always requires
real secrets, always verifies, and **has no bypass flag to pass**.

This is not speculative. The deployed stack already works this way. Seven
generated secret files sit in `/opt/zeroship-deploy/secrets`
(`auth-signing.pem`, `gateway-signing.pem`, `control-signing.pem`,
`broker-secret`, `pairwise-salt`, `refresh-hash-key`, `refresh-idem-key`) and
the services read them without complaint. Local dev is the odd one out, using
hardcoded literals. Unify by making the file-provisioned path universal and
automatic.

Generation must be coordinated, because several secrets are SHARED: gateway and
auth read one `broker-secret` file, and auth's `pairwise-salt` file content must
equal the `PAIRWISE_SALT` value control and gateway read, since all three derive
the same per-app `pws_`. One generator writing one directory satisfies both by
construction; a per-service generator would not.

Generation must also be idempotent, so restarts do not invalidate issued tokens.

## Why not a compile-time feature

The obvious Rust analogue of the auth pattern is `#[cfg(feature =
"dev-insecure")]`, so the production binary physically lacks the bypass. It is
rejected, because it reintroduces the problem it is meant to solve: dev and
production would then run DIFFERENT binaries, and the secured path would again
be exercised only in production. Moving provisioning to a separate command keeps
one binary and one code path, and the asymmetry lives in a step you run, not a
branch the server takes.

## What this deletes

- `--dev-insecure` and `ZEROSHIP_DEV_INSECURE` entirely, all 47 non-test sites
- every hardcoded weak secret: `platform-key`, `master-key`,
  `dev-worker-key-not-for-production-use`, `dev-secret-rotate-me-too`,
  `dev-stash-signing-key-not-for-production`, `dev-pairwise-salt-never-rotate-in-prod`
- the "DEV ONLY - NOT for production" banner, which is currently false
- the `ZEROSHIP_CONTROL_KEY` desync trap. It is a literal in control, gateway,
  worker and migrated but `${VAR}`-indirected in auth, so setting it today moves
  auth alone. With no literal there is nothing to disagree with.

## Enforcement

Three gates, none of which depend on anyone remembering the rule.

1. **No relaxation flag exists to set.** Deleting the flag is the enforcement
   for bucket A and B. There is no "off" to find.
2. **Unknown-variable rejection at startup.** An unrecognised `ZEROSHIP_*`
   variable is an error, not silence. This is what catches set-but-unread
   config, the shape behind three failures on 2026-08-11 and 2026-08-12:
   `PROVISION_DATABASE_URL` set on control which has no code reading it;
   `zeroship.example.toml:116` declaring `POSTMARK_WEBHOOK_PASSWORD` while the
   consumer reads `AUTH_POSTMARK_WEBHOOK_PASSWORD`; and the
   `ZEROSHIP_CONTROL_KEY` split above.
3. **`--check-config` reports posture, not just values.** Which guards are
   active, and for each secret whether it was supplied or generated. A deploy
   can then assert its own posture instead of asserting it in prose.

## Migration, ordered by risk

1. **Set `STRIPE_WEBHOOK_SECRET` on `zeroship.co`.** One env var. The code
   already verifies whenever the secret is non-empty
   (`stripe_handlers.rs:1087`). Removes the live exposure and waits on nothing
   in this proposal.
2. **Extract bucket A** into `origin_scheme` and `trusted_origins`. After this
   the deployment sets only those, and cannot silently acquire the other five
   behaviours. Blast radius: the compose command lines plus the gateway and
   control sites that read the scheme.
3. **Add `zeroship dev init` and switch compose to a generated secrets dir.**
   Local dev and the compose stack stop using literals. No server code changes.
4. **Make bucket B unconditional**, one check at a time, each with its dev input
   already provided by step 3. Deleting `insecure_dev` from a call site is only
   safe once its input exists locally, so this step follows 3 and is done
   per-check rather than in one commit.
5. **Delete the flag and the weak defaults.** Mechanical once 2 to 4 land.
6. **Add the unknown-variable gate and posture reporting.**

Pre-launch, so each step is rename-and-update-callers with no deprecation
window.

## Rejected alternatives

- **Keep the flag, document it harder.** The current banner already says DEV
  ONLY and is running in production. Documentation is what failed.
- **Keep the flag but refuse it on a non-loopback bind.** Better than today, and
  still a hatch: it fails closed only for the case someone anticipated, and
  `zeroship.co` binds `0.0.0.0` on the gateway legitimately.
- **Compile-time feature.** See above; splits the binary and under-tests the
  secured path.
- **Per-service secret generation.** Breaks the shared-secret invariants
  (`broker-secret`, `pairwise-salt`) by construction.
- **Commit dev secrets to the repo.** Removes the provisioning step and
  reintroduces shared weak keys, which is what we are deleting.

## Case against this proposal

The strongest argument against is cost against benefit at this moment. The work
is 391 non-test `insecure_dev` occurrences across five services, plus a new CLI command, plus
per-check migration in step 4 that cannot be batched. Nothing in it makes a
creator app work that does not work today. There are no users, and the exposure
it removes is on a host whose control plane is already loopback-bound, whose
only public port is the Caddy edge, and whose Stripe webhook secret can be set
in one line today.

A cheaper package would be: do step 1, do step 2, and stop. That removes the
live exposure and the reason the flag is on in production, for a fraction of the
work. Steps 3 to 6 would then be justified when there is a second deployment or
a first user, whichever comes first.

For that to be the right call, one thing has to be true: that the flag will not
be set on a future deployment for a NEW reason, the way it was set on this one
for the scheme. Given the flag remains available and still bundles four
behaviours after step 2, I do not believe that, which is why I propose the full
sequence. But the disagreement is real and the cheap package is defensible.

## What was not verified

- I did not audit all 391 non-test `insecure_dev` occurrences. The six-behaviour table comes
  from reading `main.rs` startup validation, `gateway/src/auth_token.rs`,
  `control/src/stripe_handlers.rs`, `control/src/device_handlers.rs` and
  `control/src/workflow_instance_api.rs`. There may be further categories.
- I did not check whether every check in bucket B has a satisfiable local input.
  Stripe is the known hard case: keeping verification on locally needs a signer,
  which step 4 must supply and which I have not designed.
- I did not measure how many tests set `ZEROSHIP_DEV_INSECURE`, so the blast
  radius of step 5 on the test suites is unknown.
- I have not read `docs/proposals/2026-08-11-config-name-alignment.md` in full,
  only its structure and section 2. If its `DevEnv<T>` semantics conflict with
  anything here, that proposal is the authority on naming and this one should
  be amended.
