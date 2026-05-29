# Server configuration unification

**Status:** Implemented on `feat/server-config-unification` (units U10a–U10g + a consolidation/hardening polish pass; 8 commits, unmerged). This document is the as-built design.

> Grounded in `docs/proposals/server-config-inventory.md` (whole-codebase audit, 2026-05-28). Every file:line claim below traces to that inventory.

## Goal

One config idiom across the **four web-tier server binaries** — `control`, `gateway`, `worker`, `auth-server` — replacing the three divergent flag parsers they use today (`auth` already uses the target pattern). Add a small **optional file overlay** for the handful of values that are genuinely cross-binary (the trusted-OAuth-client list, the hydra/auth URLs, observability defaults).

Explicitly **out of scope** (see "What this is NOT"): `crates/sandbox` (env-only by design, ~75 `SANDBOX_*` vars, microVM orchestration, zero auth overlap) and `crates/cli` (its own hand-rolled positional parser + `token.json` under XDG dirs).

## Problem

Three of the four binaries (`control`, `gateway`, `worker`) each re-implement the same `arg_or_env` helper verbatim (`control/main.rs:24-31`, `gateway/main.rs:342-349`, `worker/main.rs:216-223`); `control` adds `env_or`/`flag_or_env` on top. `auth` already uses `clap` derive (`AuthConfig`, 40 fields, `crates/auth/src/config.rs`) — the de-facto target. The cost of three parsers is a **drift catalog**:

| # | Drift | Where | Resolution |
|---|-------|-------|------------|
| D1 | **hydra-admin double-name** — `AUTH_HYDRA_ADMIN`/`--hydra-admin` *and* `HYDRA_ADMIN_URL`/`--hydra-admin-url`, glued by fallback at `control/main.rs:296-303` | control | Delete the `AUTH_HYDRA_ADMIN`/`--hydra-admin` alias; keep `HYDRA_ADMIN_URL`/`--hydra-admin-url` |
| D2 | **`--workers` overloaded** — CSV of worker URLs (`WORKER_URLS`) in control/gateway vs. ntex thread count (`WORKER_THREADS`) in worker | all 3 | Rename worker's flag to `--worker-threads`/`WORKER_THREADS`; `--workers`/`WORKER_URLS` then means URLs everywhere |
| D3 | **blob-root name drift** — `BUNDLES_DIR` (control) vs `BLOB_STORE` (gateway/worker), identical `./bundles` default | all 3 | Adopt `BLOB_STORE` (2 of 3 already); delete `BUNDLES_DIR` |
| D4 | **dev-insecure, three spellings** — `ZEROSHIP_DEV_INSECURE=1` (all) + `INSECURE_DEV=true` (gateway only), inconsistent truthy parsing (`"1"` vs `"true"`) | gateway | Collapse to one spelling `ZEROSHIP_DEV_INSECURE`; delete `INSECURE_DEV`. Typed clap `bool` also unifies the inconsistent truthy parsing, not just the extra spelling |
| D5 | **`STASH_SIGNING_KEY` validation asymmetry** — gateway rejects dev-default + `<32 B`; control checks presence only | control vs gateway | Apply gateway's strong check to control via a shared `validate_stash_key` |
| D6 | **`GATEWAY_OIDC_SECRET` ships an insecure default** `dev-secret-rotate-me-too`, never validated/required (`gateway/main.rs:84-89`) | gateway | Make it required-non-dev, mirroring control's `CONSOLE_OIDC_SECRET` |
| D7 | **silent numeric parsing** — every `.parse().unwrap_or(default)` swallows typos (`MAX_ISOLATES=abc` → 200) | gateway, worker | clap typed fields hard-error on bad input — free once on clap |
| D9 | **Hydra-public base, triple-named + a name collision** — the Hydra public OIDC base is `AUTH_PUBLIC` in control (`main.rs:280`), `HYDRA_PUBLIC` in gateway (`main.rs:81`), `AUTH_HYDRA_PUBLIC` in auth (`config.rs:35`); *separately*, gateway's `AUTH_PUBLIC` (`main.rs:82`, default `http://auth:9092`) names a **different** thing — the `crates/auth` UI base — colliding with control's `AUTH_PUBLIC` | control, gateway, auth | Unify the Hydra-public concept to one field `hydra_public_url`; rename gateway's auth-crate-UI `AUTH_PUBLIC` → `AUTH_UI_URL` to kill the collision |

The hydra-public consolidation in D9 is the **goal**, not an already-identical state: today the three binaries each name this base differently, and gateway overloads `AUTH_PUBLIC` for an unrelated URL. (Note also that `docs/runbooks/local-dev.md:97` documents a `--jwt-secret` flag that `control` does not have — `--jwt-secret`/`JWT_SECRET` appear zero times in `crates/`; only gateway has `--auth-secret`/`AUTH_SECRET`. That is a stale doc line to delete, **not** a config-unification item.)

Beyond drift, there is **no clean home for the trusted-OAuth-client whitelist** that P10-U10 needs — today a hardcoded const in `crates/control/src/trusted_clients.rs` (whose own comment already anticipates this move).

## Research summary

An industry survey of how other projects do startup config — *not* codebase-grounded, unlike every other claim in this doc (line 5). Sampled ~12 production Rust servers (Pingora, Vector, TiKV, Materialize, Neon, Convex, sccache, Quickwit, Linkerd2-proxy, Cargo):

- **All use `clap` derive** for CLI parsing. None use `figment`, `config-rs`, or `confique` in the startup path.
- **Most ship zero startup config files.** Files are reserved for *operator-edited domain config* (TiKV's RocksDB knobs, Vector's pipelines), not for `--port`/`--db-url`.
- **TOML is the Rust default**, not YAML (`serde_yaml` deprecated March 2024).
- **Per-field merge is hand-rolled** (~30 LOC) — Pingora, TiKV, Neon all do this; no framework.

## Proposal

### Two surfaces

**Surface A — per-binary `clap` derive.** Collapse the three `arg_or_env` parsers onto `auth`'s pattern. Shared sub-structs — a new `ObservabilityFlags` (today the three binaries call `observability::init_tracing(<literal>)` directly with no shared struct), plus the secret/URL groups — live in `crates/core/src/config.rs` and are `#[command(flatten)]`-ed in:

```rust
#[derive(Parser)]
struct ControlCli {
    #[arg(long, env = "CONTROL_PORT", default_value_t = 9090)]      port: u16,
    #[arg(long, env = "DATABASE_URL")]                              db: String,
    #[arg(long, env = "MASTER_KEY", hide_env_values = true)]        master_key: String,
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]                config_path: Option<PathBuf>,
    // file-overlayable scalar: NO default_value — see Precedence
    #[arg(long = "hydra-admin-url", env = "HYDRA_ADMIN_URL")]       hydra_admin_url: Option<String>,
    #[command(flatten)] obs: ObservabilityFlags,
}
```

**Surface B — one optional TOML overlay**, `ops/zeroship.toml`, loaded *only when* `--config`/`ZEROSHIP_CONFIG` points at it. There is no auto-discovery at a well-known path: absent that flag, the overlay is simply not loaded. It fills gaps; nothing breaks if it's absent.

```toml
# ops/zeroship.toml — cross-binary domain config. Hand-edited; in git.
# One file PER ENVIRONMENT. Secrets never live here.

[auth]
hydra_admin_url       = "http://hydra:4445"          # control + auth-server
hydra_public_url      = "https://auth.zeroship.ai"   # consolidates AUTH_PUBLIC/HYDRA_PUBLIC/AUTH_HYDRA_PUBLIC — see D9
trusted_oauth_clients = ["zeroship-builder"]         # was a const in trusted_clients.rs

[observability]
rust_log   = "info,zeroship_=debug"   # EnvFilter directive (RUST_LOG)
log_format = "json"                   # ZEROSHIP_LOG_FORMAT: pretty|compact|json|logfmt|bunyan
```

Only `[auth]` and `[observability]` ship in v1 — those are the *only* values the inventory shows as genuinely cross-binary today (§5 of the inventory). Sections are **domain-organized, not binary-organized**: `[auth]` means "the auth domain's truth," which ages better than a structural `[shared.*]`. Each binary reads only what applies:

| Binary | Reads | Notes |
|---|---|---|
| `control` | `[auth]` (hydra_admin_url, hydra_public_url, trusted_oauth_clients), `[observability]` | — |
| `gateway` | `[auth]` (hydra_public_url), `[observability]` | — |
| `worker` | `[observability]` | no auth-domain config |
| `auth-server` | `[auth]` (hydra_admin_url, hydra_public_url), `[observability]` | mostly a rename — `AuthConfig` already mirrors these |

**Future sections (deferred, not in v1):** `[runtime]` (V8 cpu/heap/wall defaults), `[control]` (deploy cap, default plan), `[billing]` (fee bps) are **new surfaces, not lifts** — the worker reads no config-driven V8 limits today (its knobs are `MAX_ISOLATES`/`POLL_INTERVAL`/`SHUTDOWN_TIMEOUT`/`WORKER_THREADS`); control's deploy cap (`MAX_COMPRESSED_BYTES`, `deploy.rs`) and default plan (`default_plan() -> "free"`, `api.rs:32`) are hard-coded consts; and billing fee bps lives in `zeroship-platform`, configured via a caller-supplied TOML path/value (`metering/config.rs::load(path: &str)` / `from_toml`), not from process env. Add each section only when something actually consumes it. YAGNI until then.

### What stays on CLI/env, never in the file

Per-deployment, per-binary, or secret:

- Ports, bind addrs, DSNs-with-creds, blob roots, thread counts, poll intervals, timeouts.
- **OAuth `audience`** — control-only (`OAUTH_AUDIENCE`, default `control.zeroship.ai`, `control/main.rs:304-309`); gateway and auth have no audience config, so it never enters the shared `[auth]` section. Stays CLI/env on control.
- **Every secret** — `MASTER_KEY`, `CONTROL_KEY`, `WORKER_KEY`, the JWT/auth secret, `STRIPE_WEBHOOK_SECRET`, `STASH_SIGNING_KEY`, `*_OIDC_SECRET`, signing-key *file paths*. Env/file-path only (the inventory confirms no secret lives in any checked-in config file today — keep that posture).
- `--dev-insecure`, `--trust-proxy`, `--config` itself.

### Precedence — and the mechanism

Per field: **CLI flag > env var > file > compiled-in default.** The mechanism matters because `clap` cannot tell a CLI/env-supplied value from a compiled `default_value`:

- A **file-overlayable scalar** becomes `Option<T>` in the clap struct with **no `default_value`**. clap leaves it `None` unless a flag or env set it. Resolution is then `cli_field.or(file_field).unwrap_or(COMPILED_DEFAULT)`.
- **Secrets and per-instance fields** stay as normal clap fields (`env = …`, with a `default_value` where appropriate) and are **never** read from the file.
- List/nested values (`trusted_oauth_clients`) are effectively **file-only** — no CLI override is practical, so they resolve straight from the file (empty `Vec` default).

```rust
// crates/core/src/config.rs

#[derive(Debug, Deserialize, Default)]
pub struct FileConfig {
    #[serde(default)] pub auth: AuthSection,
    #[serde(default)] pub observability: ObsSection,
}

#[derive(Debug, Deserialize, Default)]
pub struct AuthSection {
    pub hydra_admin_url: Option<String>,
    pub hydra_public_url: Option<String>,        // consolidates AUTH_PUBLIC/HYDRA_PUBLIC/AUTH_HYDRA_PUBLIC — see D9
    #[serde(default)] pub trusted_oauth_clients: Vec<String>,
}

impl FileConfig {
    /// Optional overlay: absent file ⇒ all-default, never an error.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        match path {
            Some(p) => Ok(toml::from_str(&std::fs::read_to_string(p)?)?),
            None => Ok(Self::default()),
        }
    }
}

// In each binary, after `Cli::parse()`:
//   let file = FileConfig::load(cli.config_path.as_deref())?;
//   let hydra_admin_url = cli.hydra_admin_url
//       .or(file.auth.hydra_admin_url)
//       .unwrap_or_else(default_hydra_admin_url);
```

Missing sections get `Default::default()`, so a single-binary deployment needs no file at all.

## Migration plan

Incremental, per-binary. Each unit ships independently; no big-bang refactor.

1. **U10a (foundation).** Add `crates/core/src/config.rs` with `FileConfig` + `AuthSection`/`ObsSection` and the `load`/resolve helpers; add `ops/zeroship.toml` with current values; mount it in compose (`--config`). Add a shared `validate_stash_key`/`validate_secret` module (resolves **D5**).
2. **U10b (control).** Consume `[auth].trusted_oauth_clients` and **delete** `crates/control/src/trusted_clients.rs`'s const; ships with P10-U10 trusted-clients hardening. Read `[auth]` URLs via the resolve helper. **Delete** the `AUTH_HYDRA_ADMIN`/`--hydra-admin` alias (**D1**). Rename control's `AUTH_PUBLIC` → `hydra_public_url` (**D9** — control's `AUTH_PUBLIC` is the Hydra-public issuer base).
3. **U10c (gateway).** Move the `[auth]` URLs it owns to file lookup with CLI/env override retained (its other auth flags are `--gateway-public-url`, `--auth-secret`). Rename `HYDRA_PUBLIC` → `hydra_public_url` and the colliding auth-crate-UI `AUTH_PUBLIC` → `AUTH_UI_URL` (**D9**). Make `GATEWAY_OIDC_SECRET` required-non-dev (**D6**); delete the `INSECURE_DEV`/`--insecure-dev` spelling (**D4**). *Caveat:* the precise source of the gateway OIDC RP's issuer (whether it sources from `hydra_public_url` or the renamed `AUTH_UI_URL`) is settled during implementation — today gateway builds its RP from `auth_public`=:9092 (`main.rs:248`) while control builds its RP from `auth_public`=:4444/hydra, so config alone doesn't decide it.
4. **U10d (auth-server).** Consume `[auth]`; mostly a rename — `AuthConfig` already mirrors this shape. Rename `AUTH_HYDRA_PUBLIC` → `hydra_public_url` (**D9**). Same RP-issuer-source caveat as U10c: the exact wiring is settled during implementation.
5. **U10e (worker).** Move to `clap` derive; rename the thread-count flag off `--workers` to `--worker-threads`/`WORKER_THREADS` so `--workers` means URLs everywhere (**D2**). Typed fields hard-error on bad numerics (**D7**). No `[runtime]` section — worker has no config-driven V8 limits today.
6. **U10f (cleanup).** Delete every bespoke `arg_or_env`/`env_or`/`flag_or_env`; adopt `BLOB_STORE` everywhere and delete `BUNDLES_DIR` (**D3**); introduce a shared `ObservabilityFlags` in `crates/core/src/config.rs` and retrofit the three `init_tracing` call sites onto it.

## What this is NOT

- **Not a hot-reload system.** No surveyed project hot-reloads config. Restart the binary.
- **Not a secret store.** Secrets stay in env vars / file paths (or future Vault/K8s Secrets), never in the TOML.
- **Not a primary config file.** The file is an *optional overlay*. The dominant supply surface today is CLI flags in compose/`tests/*.sh`/runbooks; an overlay disrupts none of it (decision #1).
- **Not per-environment overlays/layering.** One `ops/zeroship.toml` *per environment*; no merged-layer system. (Because the model is one file per environment, "in-file" and "per-environment" are not in tension — that dilemma dissolves.)
- **Not sandbox or CLI.** `crates/sandbox` is env-only by design and shares nothing with the auth domain; `crates/cli` keeps its hand-rolled positional parser + `token.json`. Both deliberately out of scope.
- **Not a framework dependency.** No `figment`/`config-rs`/`confique`. Just `clap`, `serde`, `toml`.

## Decided (was "open questions")

1. **File role = optional overlay.** Loaded only when `--config`/`ZEROSHIP_CONFIG` is given; precedence `CLI > env > file > default`. A primary-config-file model would force rewriting every compose `command:` line and test harness — rejected.
2. **Hydra/auth URLs live in the file's `[auth]` section**, CLI/env override retained. The one-file-per-environment model makes "per-environment" and "in-file" compatible. Only secrets and per-instance values (ports, bind addrs, DSNs-with-creds) stay out.
3. **`trusted_oauth_clients` is the headline file-only value** — the original motivator; moves out of the `trusted_clients.rs` const into `[auth].trusted_oauth_clients`.
4. **Sandbox is out** (env-only, microVM-domain, no auth overlap).

## Open questions

1. **`SecretString`-typed clap field.** Optional polish, not a blocker. `auth` already carries `stash_signing_key: String` in its clap struct via `env =` (`config.rs:56`) and that is fine. The only hard rule is **secrets never in the TOML file**. `secrecy` is not a workspace dependency today; if we later want a `SecretString`-typed field, add the crate then — not a blocker.
2. **Multiple files vs one.** If `ops/zeroship.toml` grows past ~200 lines, split by domain (`ops/auth.toml`, …). Defer — start with one.
3. **CLI augment vs replace for lists.** If `--trusted-client foo` ever needs to *augment* (not replace) the TOML list, that's a small hand-rolled merge. Skip until asked.
4. **Auto-discovery at a well-known default path.** Deferred — v1 loads the overlay only via explicit `--config`/`ZEROSHIP_CONFIG`.

## Estimated effort

Six units, each ~1 hour of focused work + tests. ~half a day for U10a-c (the part that gates P10-U10 trusted-clients hardening), another ~half-day for U10d-f cleanup.

## When to revisit the framework decision

Re-evaluate `confique` only if **all three** hold: the config struct exceeds ~50 fields; operators ask for doc-comment-generated templates; we actually need a layered file overlay. Until then, plain `clap` + `serde` + `toml`.
