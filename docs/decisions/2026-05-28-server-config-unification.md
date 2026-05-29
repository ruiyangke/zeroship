# ADR — Server config unification

- **Date:** 2026-05-28
- **Status:** Accepted
- **References:** `docs/proposals/server-config-unification.md`

## Context

Before this change, the web-tier server binaries had two startup-config idioms:
`control`, `gateway`, and `worker` each hand-rolled `arg_or_env` parsing, while
`auth` already used `clap` derive. The split produced a drift catalog:

- D1: duplicate Hydra-admin spelling in `control`.
- D2: `--workers` meant worker URLs in `control`/`gateway` but thread count in
  `worker`.
- D3: bundle/blob root naming drift.
- D4: multiple dev-insecure spellings and truthy rules.
- D5: asymmetric `STASH_SIGNING_KEY` validation.
- D6: `GATEWAY_OIDC_SECRET` had an insecure compiled default outside explicit
  dev mode.
- D7: numeric parse failures silently fell back to defaults.
- D8: test-only Hydra-public env naming collided with the production naming
  shape.
- D9: the Hydra-public base had three names, while `gateway` also used
  `AUTH_PUBLIC` for the auth UI base.

There was also no operator-editable home for the trusted first-party OAuth
client whitelist; it lived as a hardcoded control-plane constant.

## Decision

Use `clap` derive across the four web binaries: `zeroship-control`,
`zeroship-gate`, `zeroship-worker`, and `zeroship-auth`.

Add one optional TOML overlay, normally `ops/zeroship.toml`, loaded only via
`--config` or `ZEROSHIP_CONFIG`. The overlay is domain-organized and currently
contains only `[auth]` and `[observability]`. Scalar precedence is:

`CLI flag > env > file > compiled default`

The mechanism is explicit: file-overlayable CLI fields are `Option<T>` with no
compiled clap default, then resolved in `crates/core/src/config.rs`. Shared
helpers also live there, including overlay loading, observability resolution,
secret-presence guards, and stash-key validation.

Secrets never live in the TOML file. They remain env, CLI, or file-path inputs.

Every web binary accepts `--check-config`, which validates CLI/env/file config
and startup guards, prints resolved non-secret config, and exits without
starting the server.

Move the trusted first-party OAuth-client whitelist from a hardcoded
control-plane constant into `[auth].trusted_oauth_clients`, with the compiled
default retained only as the absent-file default.

Keep `crates/sandbox` and `crates/cli` out of scope. The sandbox remains
env-only in the microVM orchestration domain; the CLI keeps its positional
parser and XDG `token.json` state.

## Rejected Options

- `figment`, `config-rs`, or `confique`: rejected in favor of plain
  `clap` + `serde` + `toml`, matching the Rust-server survey in the proposal.
- A primary or required config file: rejected because the existing dev and
  compose stack is CLI-flag-driven. The file is an optional overlay.
- YAML: rejected because TOML is the Rust default here and `serde_yaml` is
  deprecated.
- `[runtime]`, `[control]`, and `[billing]` sections: deferred as YAGNI. They
  are new surfaces, not values currently consumed from process config.
- Well-known-path auto-discovery: now implemented in this branch.
  `FileConfig::resolve()` probes the fixed system path
  `/etc/zeroship/zeroship.toml` when no explicit `--config`/`ZEROSHIP_CONFIG` is
  given. The system path is the only auto-discovered location (no CWD, no
  env-redirect, for hardening); an explicit path that fails is a hard error,
  while a *missing* well-known path falls back to all-defaults.
- Unifying sandbox or CLI config: rejected as out of scope for this web-tier
  server change.

## Consequences

The deploy stack can define shared auth-domain URLs, trusted OAuth clients, and
observability defaults once. `docker-compose.yml` dogfoods this by mounting the
overlay for `control` and `gateway`.

Operators get an `nginx -t`-style dry run through `--check-config` before
starting a binary.

Future cross-binary config has a clear home, but new TOML sections should be
added only when a binary actually consumes them.

The as-built reference remains `docs/proposals/server-config-unification.md`.
