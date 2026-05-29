# Config-system fix plan — target shapes + task order (2026-05-29)

> **Status: IMPLEMENTED** (commits `ea7d740b`, `c44529a7`, `f33f4bb5`). All phases landed;
> per-item disposition is in the resolution table of `2026-05-29-config-system-critique.md`.
> O3 (structured invalid-filter warning) is now done too. The full `zeroship-config`
> crate extraction was deliberately deferred in favor of the in-`core` submodule split.

Implements every finding in `2026-05-29-config-system-critique.md`. Pre-launch: rename/delete,
**no shims, no @deprecated, no back-compat**. Every behavioral fix gets a regression test that
would fail pre-fix. Commit-only, **never push**.

This file is the authoritative contract. Implementation agents MUST converge on these exact
shapes. It is committed alongside the implementing changes.

---

## New `core` module layout (M8 — split the god-module)

`crates/core/src/config.rs` → `crates/core/src/config/` :

- **`mod.rs`** — declares submodules + `pub use` re-exports so `zeroship_core::config::{…}` keeps
  resolving for every existing name that survives.
- **`file.rs`** — `FileConfig`, `AuthSection`, `ObsSection`, `ConfigError`.
  - All three sections get `#[serde(deny_unknown_fields)]` (S7) — typos now fail loudly.
  - `AuthSection.trusted_oauth_clients: Option<Vec<String>>` (M4): `None` = use compiled default
    set; `Some(vec)` = exactly that set (empty = no trusted clients).
- **`source.rs`** — discovery.
  - `pub const SYSTEM_CONFIG_PATH: &str = "/etc/zeroship/zeroship.toml";`
  - `pub enum ConfigSource { None, Explicit(PathBuf), Discovered(PathBuf) }` with a `Display` impl
    (replaces `ResolvedConfig{source,discovered}` + `describe_source`; invalid states unrepresentable — M4).
  - `pub struct LoadedOverlay { pub config: FileConfig, pub source: ConfigSource }`.
  - `FileConfig::load(Option<&Path>) -> Result<Self, ConfigError>` (explicit-only primitive, unchanged).
  - `FileConfig::resolve(explicit: Option<&Path>, allow_discovery: bool) -> Result<LoadedOverlay, ConfigError>`
    delegating to `resolve_with_well_known(explicit, allow_discovery, well_known: &Path)`.
    - explicit `Some` → load (missing/broken = hard error), `ConfigSource::Explicit`.
    - explicit `None` + `allow_discovery` false → defaults, `ConfigSource::None` (this is the `--no-config` path, O5).
    - explicit `None` + discovery: `try_exists()` `Ok(true)` → load (broken = hard error), `Discovered`;
      `Ok(false)` → defaults/`None`; **`Err` → `warn!` + defaults/`None`** (S5: never fatal for a path
      nobody requested — fixes the fleet-wide-DoS edge; explicit path stays fatal).
  - `pub fn load_overlay(explicit, allow_discovery, binary) -> Result<LoadedOverlay, ConfigError>`
    — **returns `Result`; no `process::exit` in core** (M8). Binaries own the exit.
  - `pub fn log_overlay_source(source: &ConfigSource)` — post-tracing startup line.
- **`env.rs`** — `env_is_exact`, `env_is_truthy`, and `pub fn parse_bool_flag(s: &str) -> Result<bool, String>`
  accepting `1/0/true/false/yes/no` case-insensitively — the ONE truthiness used by every bool env flag (M3, S1).
- **`secrets.rs`** — security policy (moved out of the file/plumbing module):
  - `pub const DEV_STASH_SIGNING_KEY`.
  - `pub fn validate_stash_key(value: &str, insecure_dev: bool) -> Result<(), String>` — the single
    stash validator: exact-match dev sentinel, non-empty, raw len ≥ 32. (Deletes auth's `starts_with` copy — S4.)
  - `pub fn require_unless_dev(label, value, insecure_dev) -> Result<(), String>`.
  - `pub fn decoded_key_len(value: &str) -> Option<usize>` + `pub fn validate_key_material(label, value, insecure_dev)`
    — moved verbatim from `control/main.rs` (M3); soften error text from "random bytes" to "bytes" (O4).
  - `pub fn is_loopback_url(url: &str) -> bool` — **literal-only**: accepts `localhost` or an IP that
    `is_loopback()`; **no DNS resolution** (S8). Used by both auth and control Hydra-admin guards.

`crates/core/src/observability.rs` gains the obs-config that was wrongly living in / imported-back-into config:
- `pub struct ObservabilityFlags` (clap::Args: `--log-filter`/`RUST_LOG`, `--log-format`/`ZEROSHIP_LOG_FORMAT`)
  — `log_format: Option<LogFormat>` parsed by clap (invalid value = clap parse error, fatal — S6).
- `pub enum LogFormat { Pretty, Compact, Json, Logfmt, Bunyan }` + `FromStr` (fatal on unknown).
- `pub fn resolve_observability(flags, file: &ObsSection, default_filter) -> Result<(String, Option<LogFormat>), ConfigError>`
  — file-provided `log_format` is parsed and **errors** on invalid (S6); `log_filter` keeps validate-or-default
  but the fallback is surfaced (see O3).
- `resolve_log_filter` moves here (no more backwards import from config).
- `init_tracing_with(filter: &str, format: Option<LogFormat>)`.

`crates/core/src/config/bootstrap.rs` — the de-duplicated boot dance (M8) + structured check-config (M2):
- `pub struct Bootstrap { pub overlay: LoadedOverlay, pub log_filter: String, pub log_format: Option<LogFormat> }`
- `pub fn bootstrap(config_path: Option<&Path>, allow_discovery: bool, obs: &ObservabilityFlags,
   default_filter: &str, binary: &str) -> Bootstrap` — loads overlay (exit-on-error here, at the binary
   boundary via a thin wrapper that calls `load_overlay` + exits), resolves obs, `init_tracing_with`, then
   `log_overlay_source`. Returns everything the binary needs. Each `main` shrinks to one call.
- `pub enum CheckValue { Plain(String), Flag(bool), Count(usize), Secret(bool) }`
- `pub struct CheckConfigReport { … }` with `.field(key: &str, v: CheckValue)` and
  `.emit(format: CheckFormat)` → text (`check-config: key = value`, default) or `json`. Each binary builds
  its rows through this; no more copy-pasted `println!` blocks.

---

## Per-binary changes (Phase 2)

All four: adopt `bootstrap()` + `CheckConfigReport`; add `hide_env_values = true` to the DSN field (S2);
**drop `#[derive(Debug)]`** from the CLI struct (or add a manual redacting Debug if a test needs it) (S2);
replace the dev-insecure pattern (see below); print the check-config block via the shared report.

**dev-insecure / trust-proxy (S1, M3) — replace the SetTrue-flag + skipped-env-field pair with one field:**
```rust
#[arg(long = "dev-insecure", env = "ZEROSHIP_DEV_INSECURE",
      num_args = 0..=1, default_missing_value = "true",
      value_parser = zeroship_core::config::env::parse_bool_flag)]
dev_insecure: Option<bool>,
```
Resolve `let insecure_dev = cli.dev_insecure.unwrap_or(false);` — CLI presence overrides env, so
`--dev-insecure=false` now disables a stray `ZEROSHIP_DEV_INSECURE=1` (fixes the un-overridable precedence).
Same pattern for `--trust-proxy`. Delete the `insecure_dev()`/`trust_proxy()` accessors and the
`*_env` skipped fields.

**--bind loopback default (S1):**
- control + gateway gain `--bind`/`<PFX>_BIND` (default `127.0.0.1`); auth `--addr` default → `127.0.0.1:9092`.
- When the resolved bind is non-loopback AND `insecure_dev`, emit a loud `warn!` (compose dev intentionally
  uses `0.0.0.0` + dev-insecure on a private network, so don't hard-refuse — but make it shout).

**WORKER_KEY symmetry (S3):** gateway + control call
`require_unless_dev("WORKER_KEY / --worker-key", &worker_key, insecure_dev)` and exit on error (control has
no check today; gateway only warns).

**Hydra-admin loopback guard (S8):** control validates `hydra_admin_url` via `secrets::is_loopback_url`
(literal-only), gated by `--allow-remote-hydra-admin` exactly like auth. auth swaps its DNS-resolving check
for the shared `is_loopback_url`.

**control specifics:**
- Move the `if check_config { report…; return Ok(()) }` block **before** the deploy_tmp probe + PAT
  signing-key load so `--check-config` is side-effect-free (M1).
- `validate_master_key_material`/`decoded_master_key_len` now live in `core::config::secrets` (call through).

**gateway specifics:**
- Delete `--auth-secret`/`AUTH_SECRET` and every use (M6) — verify `router/auth.rs` no longer needs it;
  remove the `AppState.auth_secret` field. No shim.
- Parse `--workers`/`WORKER_URLS` once into `Vec<Url>` (reject empty entries); reuse for both check-config
  count and runtime (M7).
- Compiled `DEFAULT_HYDRA_PUBLIC_URL` → prod `https://auth.zeroship.ai` (matches auth/control); compose
  supplies the internal `http://hydra:4444` via the overlay (M5).

**auth specifics (S4 — actually unify):** switch to `--dev-insecure`/`ZEROSHIP_DEV_INSECURE`; route overlay
through the shared resolver (drop bespoke `resolve_file_overlay` merge in favor of the same `resolve_overlay_string`
primitive where possible); use `core::config::secrets::validate_stash_key`; stash key `default_value = ""`
+ code dev-fallback (no secret in `--help`); `AUTH_STASH_SIGNING_KEY` → align naming.

---

## Compose / docs / ops (Phase 3)

- **O1 docker-compose.yml:** add the missing `auth:` service (gateway already targets `http://auth:9092`);
  mount the bundle volume + pass `--blob-store` to gateway & worker; mount the overlay into worker;
  pass `--bind 0.0.0.0` / `--addr 0.0.0.0:9092` now that loopback is the default.
- **O2 runbooks:** make `local-dev.md` commands actually boot (valid key material or intentional
  `--dev-insecure`; include required OIDC/auth fields); remove the `--auth-secret dev-jwt-secret` line;
  fix `docker-compose.md:36` ("not wired to this overlay" → "not mounted; would auto-discover if mounted").
- **O5 `--no-config`:** add to all four binaries (disables auto-discovery → compiled defaults even if the
  well-known file exists); wire into `bootstrap(allow_discovery = !no_config)`.
- **O3:** surface the invalid-filter fallback after tracing init (re-log via `warn!`), not only pre-tracing `eprintln!`.
- **O6:** reconcile the dangling **D8** drift ID between the ADR and the proposal table.
- `ops/zeroship.example.toml` / `ops/zeroship.toml`: document the compose-internal vs prod `hydra_public_url`
  divergence; align `trusted_oauth_clients` shape with the new `Option<Vec>`.

---

## Task / commit order

1. **Phase 1 (core contract):** module split + all new primitives + obs move + bootstrap/report + Result-not-exit.
   Tests for: ConfigSource Display, deny_unknown_fields rejection, Option<Vec> trusted-clients semantics,
   parse_bool_flag, validate_stash_key unification, is_loopback_url literal-only (incl. a DNS-name that
   resolves to loopback is REJECTED), LogFormat FromStr fatal, resolve_observability invalid-format error,
   discovered try_exists-Err → defaults. `cargo test -p zeroship-core --lib` green.
2. **Phase 2a control**, **2b gateway**, **2c worker**, **2d auth** — adopt contract + per-binary security fixes;
   each with regression tests (CLI-overrides-env dev-insecure; missing-WORKER_KEY fatal non-dev; non-loopback
   Hydra rejected; read-only check-config; gateway worker-URL parse; auth unified flag). Build each binary +
   `config_check_e2e.sh`.
3. **Phase 3** compose/docs/ops + `--no-config` + D8.
4. **Phase 4** final adversarial review (security re-check of S1/S2/S3/S8), full build + tests + e2e, update the
   critique doc marking items fixed.
