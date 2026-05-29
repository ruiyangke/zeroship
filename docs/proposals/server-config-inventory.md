# Configuration inventory — whole codebase

**Status:** Reference audit (2026-05-28). Companion to `docs/proposals/server-config-unification.md`. Catalogs every configuration input — CLI flag, env var, config file, compile-time `env!` — across all Rust crates, the Go ch-driver, the TS apps/SDKs, and the ops/deploy tooling.

**Method.** Six parallel audit agents, one per slice (web-tier binaries · auth · sandbox+agent · lib crates/CLI/platform · TS apps/SDKs · ops/infra/Go). Each ran exhaustive ripgrep over its scope and read every hit for default/type/semantics. Cross-checked against a ground-truth grep of the whole tree (`env::var`/`env::var_os`/`std::env::var`, clap `env =`, `env!`/`option_env!`, `process.env`/`import.meta.env`, Go `os.Getenv`).

**Scale.** ~80 distinct Rust `env::var` names + 39 clap `env =` flags + 5 compile-time `env!` + the builder app's ~20 `process.env` + Hydra/compose env + 2 Go-driver env vars. The single largest surface is the **sandbox controller (~75 `SANDBOX_*` vars, env-only)**; the next is **auth (40 clap fields)**.

---

## Executive synthesis — what this means for unification

### 1. Four different config idioms already coexist

| Idiom | Crates | Shape |
| --- | --- | --- |
| **clap `#[derive(Parser)]`** | `auth` | 40 fields, `CLI+env`, the de-facto target pattern |
| **hand-rolled `arg_or_env`** | `control`, `gateway`, `worker` | each re-implements the same helper verbatim; `control` adds `env_or`/`flag_or_env` |
| **env-only `from_env` + `parse_env`** | `sandbox`, `sandbox-agent` | ~75 `SANDBOX_*` vars, no CLI, heavy validation |
| **hand-rolled positional** | `cli` | `std::env::args()` match, no clap, own `token.json` config file |

The unification's real job is collapsing idiom #2 (control/gateway/worker) onto idiom #1 (auth's clap pattern) and adding a shared file layer. Idioms #3 and #4 are largely out of scope (see §4).

### 2. The real supply surface is CLI flags, not files (informs decision #1: file role)

In `docker-compose.yml`, `tests/*.sh`, and the runbooks, the three core binaries are configured **almost entirely via `command:` CLI flags** — `--port`, `--db`, `--control-key`, `--master-key`, `--hydra-admin-url`, etc. Env vars are confined to the sandbox controller, the builder app, Hydra, and the Go driver's two binary-path overrides. There is **no structured config file** read by any of control/gateway/worker today; the only files consumed are individual secret/key files (PAT signing key, wrapper signing key) and the auth `ClientsConfig` TOML.

**Implication:** an *optional overlay* file (loaded only when present/pointed-at, with `CLI/env > file > default` precedence) fits the existing deploy tooling with zero disruption — nothing breaks if the file is absent. A *primary config file* would require rewriting every compose `command:` line and every test harness.

### 3. The drift catalog (the concrete payoff of unification)

Same concept, divergent spelling/semantics across binaries — every one is a unification target:

- **hydra-admin double-name (within `control`):** `AUTH_HYDRA_ADMIN` (`--hydra-admin`) *and* `HYDRA_ADMIN_URL` (`--hydra-admin-url`), glued by fallback logic at `control/main.rs:296-303`. Pure legacy alias.
- **`--workers` overloaded:** `WORKER_URLS` (CSV of URLs) in control/gateway vs. `WORKER_THREADS` (ntex thread count) in worker. **Same flag, different meaning.**
- **blob root name drift:** `BUNDLES_DIR` (control) vs `BLOB_STORE` (gateway/worker), identical `./bundles` default.
- **`DATABASE_URL` semantics:** live default `postgres://localhost/zeroship` (control) vs `""`→degraded (gateway/worker).
- **dev-insecure: three spellings:** `ZEROSHIP_DEV_INSECURE=1` (all) + `INSECURE_DEV=true` (gateway only); inconsistent truthy parsing (`"1"` vs `"true"` vs `flag_or_env`'s `"1"|"true"`).
- **`STASH_SIGNING_KEY` validation asymmetry:** gateway rejects dev-default + `<32 B`; control checks presence only. Same secret, weaker rule in one binary.
- **`GATEWAY_OIDC_SECRET` ships an insecure default** (`dev-secret-rotate-me-too`), never validated/required — a prod gateway with it unset silently runs on a known secret. (control's `CONSOLE_OIDC_SECRET` is required non-dev.)
- **`AUTH_HYDRA_PUBLIC` (server/issuer) vs `AUTH_HYDRA_PUBLIC_URL` (test-only)** — name-collision trap.
- **Hydra-public base, triple-named + a name collision (D9):** the Hydra public OIDC base is `AUTH_PUBLIC` in control (`main.rs:280`), `HYDRA_PUBLIC` in gateway (`main.rs:81`), and `AUTH_HYDRA_PUBLIC` in auth (`config.rs:35`) — three names for one concept. Separately, gateway's `AUTH_PUBLIC` (`main.rs:82`, default `http://auth:9092`) names a *different* thing, the `crates/auth` UI base, **colliding** with control's `AUTH_PUBLIC`. Unify the concept to `hydra_public_url`; rename gateway's auth-UI `AUTH_PUBLIC` → `AUTH_UI_URL`. (There is *no* `--jwt-secret` code drift: `--jwt-secret`/`JWT_SECRET` appear zero times in `crates/`; only gateway has `--auth-secret`/`AUTH_SECRET`. The `--jwt-secret` mention in `local-dev.md:97` is a stale doc line to delete, not a code drift.)
- **silent numeric parsing:** every `.parse().unwrap_or(default)` swallows typo'd values (`MAX_ISOLATES=abc` → 200 silently).

### 4. Sandbox is its own world (informs decision #4: scope it out)

`crates/sandbox` is **env-only by deliberate design** (deployed via Nomad/k8s/GCE-metadata where env is the native surface), ~75 `SANDBOX_*` vars across `config.rs`/`db.rs`/`sweep.rs`/`persist.rs`/`lib.rs`, with extensive security-critical validation (nomad-addr loopback guard, zeroizing `ApiToken`, fail-closed feature flags, mode-0400 key-file checks, HA-lease invariants). It shares **nothing** with the auth domain — no hydra, no JWT, no trusted clients. The proposal's claim that sandbox reads `[runtime]` is wrong (that section is V8 app limits; sandbox orchestrates microVMs). **Recommendation stands: scope sandbox out** of the clap-derive unification. (It could optionally adopt the shared `[observability]` knobs later — `RUST_LOG` + `ZEROSHIP_LOG_FORMAT` are already its only shared surface.)

### 5. What is genuinely cross-binary (the file's legitimate contents)

Very little, which argues for a small `[auth]`-centric file:

- **`trusted_oauth_clients`** (list) — the original motivator; today a hardcoded const in `control/trusted_clients.rs`. **File-only** (no CLI override needed).
- **Hydra-public base** — shared by control + gateway + auth, but under **three different names** (`AUTH_PUBLIC`/`HYDRA_PUBLIC`/`AUTH_HYDRA_PUBLIC` — see §3 D9) → unify as `hydra_public_url`. Per-environment.
- **`hydra_admin_url`** — shared by control + auth-server (not gateway). Per-environment.
- **OAuth `audience` is *not* shared** — it is control-only (`OAUTH_AUDIENCE`, `main.rs:304-309`); gateway and auth have no audience config. There is no standalone `issuer` config field anywhere (control's `AUTH_PUBLIC` *is* the Hydra-public issuer base). So neither `issuer` nor `audience` belongs in the shared `[auth]` section.
- **observability** — `RUST_LOG` (via `EnvFilter`) + `ZEROSHIP_LOG_FORMAT`, already read by every binary through `core/observability.rs`. Natural shared `[observability]` section.
- Everything else (ports, DSNs, secrets, blob roots, thread counts, poll intervals, timeouts) is **per-deployment or per-binary** → stays CLI/env.

### 6. Secrets posture is already correct — keep it

No secret lives in a checked-in config file today; production secrets flow via env vars, `EnvironmentFile=` (chmod 0400), or file *paths* to mode-checked key files. The unification must preserve this: **secrets stay env/file-path only, never in the TOML.**

### 7. Test-only vars are cleanly separable

A large fraction of `env::var` hits are test-gating (`PG_TEST_URL`, `REDIS_TEST_URL`, `DRAGONFLY_CLUSTER_SEEDS`, `AUTH_DB_URL`-as-skip-guard, `AUTH_LOAD_TEST`, `KV_REQUIRE_REDIS`, the `ZEROSHIP_SANDBOX_TEST_*` overrides, `CARGO_*` compile-time) or test-helpers-gated (`ZEROSHIP_SESSION_SECRET*`, only compiled with `feature = "test-helpers"`). None are production config and none should enter the unified surface — each is tagged `(test-only)` in the sections below.

---

## Completeness cross-check

Ground-truth = a whole-tree grep for `env::var`/`env::var_os`/`std::env::var`, clap `env =`, `env!`/`option_env!`, plus the Go driver's `os.Getenv` (81 distinct names). Each was whole-word-matched against this document: **78 of 81 are tabled in the sections below.** The 3 not individually tabled are all test/stress-only and deliberately out of scope:

- `CONTROL_TEST_DB` — test DSN selector in `crates/control/tests/stripe_webhook_test.rs` (test-only; alongside `PG_TEST_URL`/`AUTH_DB_URL`).
- `KUBECONFIG` — standard kubectl config path consumed by the k8s backend / its tests, not zeroship config.
- `SBX_STRESS_NAMESPACE` — sandbox stress-harness namespace override (test/stress-only).

(The builder app's `process.env` surface, Hydra's compose env, and the deploy-script env are additionally catalogued in the TS and Ops sections and are not part of the 81-name Rust/Go ground truth.)

---
## Web-tier binaries (control, gateway, worker)

| Name | Kind | Type | Default | Secret? | Refuses boot if unset? | Controls | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **control** | | | | | | | |
| `--port` / `CONTROL_PORT` | CLI+env | u16 (string) | `9090` | no | no | HTTP listen port (`0.0.0.0:{port}`) | `crates/control/src/main.rs:86` |
| `--db` / `DATABASE_URL` | CLI+env | string (DSN) | `postgres://localhost/zeroship` | no (DSN may embed creds) | no | Control-plane Postgres connection (`Registry::new`); also dev fallback for auth DB | `crates/control/src/main.rs:87` |
| `--bundles` / `BUNDLES_DIR` | CLI+env | path | `./bundles` | no | no | Root for both legacy `BundleStore` (`LocalFs`) and new `LocalDiskBlobStore` | `crates/control/src/main.rs:88` |
| `--control-key` / `CONTROL_KEY` | CLI+env | string | `""` | **yes** | **yes** (unless `--dev-insecure`) | Bearer key for admin + internal API auth | `crates/control/src/main.rs:89`; guard `:179`, `:184-192` |
| `--master-key` / `MASTER_KEY` | CLI+env | string (hex/base64url ≥32 B) | `""` | **yes** | **yes** (unless `--dev-insecure`); also rejected if decodes <32 B | Env-var encryption key (`EnvStore`) | `crates/control/src/main.rs:90`; validate `:193-197`; guard `:175-192` |
| `--workers` / `WORKER_URLS` | CLI+env | CSV of URLs | `http://localhost:8080` | no | no | Worker dispatch URLs (split on `,`) | `crates/control/src/main.rs:91` |
| `--worker-key` / `WORKER_KEY` | CLI+env | string | `""` | **yes** | no (silently empty) | Shared secret control uses when calling workers | `crates/control/src/main.rs:92` |
| `--signing-key-file` / `SIGNING_KEY_FILE` | CLI+env | path | `""` | **yes** (path to key) | **yes** (unless `--dev-insecure`; else dev PAT key) | PAT (personal-access-token) signing key file | `crates/control/src/main.rs:93`; guard `:181-192`; load `:223-238` |
| `--stripe-webhook-secret` / `STRIPE_WEBHOOK_SECRET` | CLI+env | string | `""` | **yes** | no (warns only; `/internal/webhooks/stripe` 500s) | Stripe webhook signature verification | `crates/control/src/main.rs:94`; warn `:208-217` |
| `--legacy-master-keys` / `LEGACY_MASTER_KEYS` | CLI+env | CSV of keys | `""` | **yes** | no (but each entry validated ≥32 B when non-dev) | Previous master keys tried on decrypt failure during rotation grace | `crates/control/src/main.rs:97`; validate `:198-207` |
| `--dev-insecure` / `ZEROSHIP_DEV_INSECURE` | CLI+env | bool (`1` only for env) | `false` | no | n/a | Master switch disabling all secret-presence guards + admin/internal auth | `crates/control/src/main.rs:102-104` |
| `--trust-proxy` / `TRUST_PROXY` | CLI+env | bool (`1` only for env) | `false` | no | no | Trust `X-Forwarded-For` for client IP | `crates/control/src/main.rs:113-115` |
| `--bootstrap-builder-client` / `BOOTSTRAP_BUILDER_OAUTH_CLIENT` | CLI+env | bool (`1`/`true` for env) | `false` | no | no | Run auth migrations + bootstrap builder OAuth client at boot | `crates/control/src/main.rs:116-120`; use `:401-420` |
| `--builder-redirect-uri` / `BUILDER_REDIRECT_URI` | CLI+env | URL | `http://localhost:3001/auth/callback` | no | no | Redirect URI for bootstrapped builder OAuth client | `crates/control/src/main.rs:121-126`; const `crates/control/src/bootstrap_builder.rs:17` |
| `--builder-client-secret-file` / `BUILDER_CLIENT_SECRET_FILE` | CLI+env | path | `data/builder-client-secret` | **yes** (path to secret) | no | Where builder OAuth client secret is written/read | `crates/control/src/main.rs:127-132`; const `crates/control/src/bootstrap_builder.rs:18` |
| `--deploy-tmp-dir` / `DEPLOY_TMP_DIR` | CLI+env | path | `""` → `std::env::temp_dir()` | no | **yes** (exits if dir not creatable/writable — probe at boot) | Scratch dir for deploy unpacking | `crates/control/src/main.rs:134-144`; probe `:150-171` |
| `--auth-public` / `AUTH_PUBLIC` | CLI+env | URL | `""` → dev `http://localhost:4444` | no | **yes** (unless `--dev-insecure`) | Hydra public issuer URL for console OIDC RP | `crates/control/src/main.rs:280`; guard `:313-314`; dev fallback `:346-352` |
| `--hydra-admin` / `AUTH_HYDRA_ADMIN` | CLI+env | URL | `""` | no | no directly (feeds `hydra_admin_url` fallback) | **Legacy** hydra-admin env name; used only if `--hydra-admin-url`/`HYDRA_ADMIN_URL` empty | `crates/control/src/main.rs:281-286`; fallback `:296-303` |
| `--hydra-admin-url` / `HYDRA_ADMIN_URL` | CLI+env | URL | `""` → (falls back to `AUTH_HYDRA_ADMIN`) → dev `http://localhost:4445` | no | **yes** (effective value; unless `--dev-insecure`) | Hydra admin URL for OAuth bearer introspection + builder bootstrap | `crates/control/src/main.rs:296-303`; guard `:325-326`; dev fallback `:353-359` |
| `--console-oidc-secret` / `CONSOLE_OIDC_SECRET` | CLI+env | string | `""` → dev `dev-console-oidc-secret` | **yes** | **yes** (unless `--dev-insecure`) | OIDC RP client secret for `console.zeroship.ai` | `crates/control/src/main.rs:287-288`; guard `:316-317`; dev fallback `:360-364` |
| `--stash-signing-key` / `STASH_SIGNING_KEY` | CLI+env | string | `""` → dev `dev-stash-key-please-rotate` (literal `b"..."`) | **yes** | **yes** (unless `--dev-insecure`) | HMAC key for the OIDC RP login-stash cookie | `crates/control/src/main.rs:289-294`; guard `:319-320`; dev fallback `:338-345` |
| `--auth-db` / `AUTH_DB_URL` | CLI+env | string (DSN) | `""` → dev falls back to `db_url` | no (DSN may embed creds) | **yes** (unless `--dev-insecure`) | Auth/console Postgres (`/auth/callback`, sessions) | `crates/control/src/main.rs:295`; guard `:322-323`; dev fallback `:379-387` |
| `--oauth-audience` / `OAUTH_AUDIENCE` | CLI+env | string | `control.zeroship.ai` | no | no | Expected `aud` for OAuth bearer introspection | `crates/control/src/main.rs:304-309` |
| **gateway** | | | | | | | |
| `--port` / `GATE_PORT` | CLI+env | u16 (string) | `80` | no | no | HTTP listen port (`0.0.0.0:{port}`) | `crates/gateway/src/main.rs:65` |
| `--control` / `CONTROL_URL` | CLI+env | URL | `http://localhost:9090` | no | no | Control-plane base URL (route/version sync) | `crates/gateway/src/main.rs:66` |
| `--control-key` / `CONTROL_KEY` | CLI+env | string | `""` | **yes** | **yes** (unless any dev-insecure form) | Bearer key for pulling routes from control | `crates/gateway/src/main.rs:67`; validate `:28-33`, `:113-116` |
| `--workers` / `WORKER_URLS` | CLI+env | CSV of URLs | `http://localhost:8080` | no | no | Worker URLs for CHWBL hash ring (split on `,`) | `crates/gateway/src/main.rs:68` |
| `--poll-interval` / `POLL_INTERVAL` | CLI+env | u64 secs (string) | `5` | no | no | Route/version poll cadence (`.parse().unwrap_or(5)`) | `crates/gateway/src/main.rs:69`; parse `:268` |
| `--auth-secret` / `AUTH_SECRET` | CLI+env | string | `""` | **yes** | no | JWT validation secret (gateway auth) | `crates/gateway/src/main.rs:70` |
| `--worker-key` / `WORKER_KEY` | CLI+env | string | `""` | **yes** | no (warns: worker endpoints unauthenticated) | Shared secret for `/dispatch` bearer + `ZeroShip-User` HMAC | `crates/gateway/src/main.rs:71`; warn `:128-132` |
| `--blob-store` / `BLOB_STORE` | CLI+env | path | `./bundles` | no | no (panics if init fails) | Content-addressed blob store root (`LocalDiskBlobStore`) | `crates/gateway/src/main.rs:72`; init `:185-188` |
| `--blob-cache-mem-mb` / `BLOB_CACHE_MEM_MB` | CLI+env | usize MB (string) | `256` | no | no | In-memory blob LRU size (`.parse().unwrap_or(256)` × 1 MiB) | `crates/gateway/src/main.rs:73`; parse `:177-180` |
| `--blob-cache-disk-gb` / `BLOB_CACHE_DISK_GB` | CLI+env | u64 GB (string) | `20` | no | no | On-disk blob LRU size (`.parse().unwrap_or(20)` × 1 GiB) | `crates/gateway/src/main.rs:74`; parse `:181-184` |
| `--blob-cache-disk-root` / `BLOB_CACHE_DISK_ROOT` | CLI+env | path | `./blob-cache` | no | no (panics if init fails) | On-disk blob cache directory | `crates/gateway/src/main.rs:75-80`; init `:189-193` |
| `--hydra-public` / `HYDRA_PUBLIC` | CLI+env | URL | `http://hydra:4444` | no | no | Upstream Hydra public OIDC endpoints (proxied) | `crates/gateway/src/main.rs:81` |
| `--auth-public` / `AUTH_PUBLIC` | CLI+env | URL | `http://auth:9092` | no | no | Upstream `crates/auth` UI/consent (proxied); also OIDC RP issuer | `crates/gateway/src/main.rs:82`; RP `:248` |
| `--db` / `DATABASE_URL` | CLI+env | string (DSN) | `""` | no (DSN may embed creds) | no (warns: session validation disabled → 401) | Postgres for per-origin session store + PG DPoP jti cache | `crates/gateway/src/main.rs:83`; empty-path `:226-242` |
| `--gateway-oidc-secret` / `GATEWAY_OIDC_SECRET` | CLI+env | string | `dev-secret-rotate-me-too` | **yes** | no (insecure default ships) | OIDC RP client secret for `{app}.zeroship.ai` hosts | `crates/gateway/src/main.rs:84-89` |
| `--stash-signing-key` / `STASH_SIGNING_KEY` | CLI+env | string (≥32 B) | `""` → `dev-stash-key-please-rotate` | **yes** | **yes** (unless dev-insecure); rejects dev-default + <32 B | HMAC key for OIDC RP login-stash cookie | `crates/gateway/src/main.rs:90-95`; validate `:35-58`, `:118-126`; const `:18` |
| `--dev-insecure` / `ZEROSHIP_DEV_INSECURE` (=`1`) **or** `--insecure-dev` / `INSECURE_DEV` (=`true`) | CLI+env | bool | `false` | no | n/a | Dev mode: cookies without `Secure`, relaxes control-key + stash-key guards. **TWO distinct env names accepted** | `crates/gateway/src/main.rs:20-26` |
| `--trust-proxy` / `TRUST_PROXY` | CLI+env | bool (`1` only for env) | `false` | no | no | Trust `Forwarded`/`X-Forwarded-For` for client IP | `crates/gateway/src/main.rs:97-99` |
| `--signing-key-file` / `GATEWAY_SIGNING_KEY_FILE` | CLI+env | path (PKCS#8 PEM/DER) | `""` | **yes** (path to key) | no (warns: DPoP-exchange 503; panics if file load fails) | Wrapper-token Ed25519 signing key (DPoP exchange) | `crates/gateway/src/main.rs:100-105`; load `:138-153` |
| `--gateway-public-url` / `GATEWAY_PUBLIC_URL` | CLI+env | URL | `https://api.zeroship.ai` | no | no | `iss` advertised in gateway wrapper tokens (must be stable) | `crates/gateway/src/main.rs:106-111` |
| **worker** | | | | | | | |
| `--port` / `WORKER_PORT` | CLI+env | u16 (string) | `8080` | no | no | HTTP listen port (`{bind_host}:{port}`) | `crates/worker/src/main.rs:65` |
| `--workers` / `WORKER_THREADS` | CLI+env | usize (string) | `available_parallelism()` (else `1`) | no | no | ntex worker thread count (`.parse().unwrap_or(1)`) | `crates/worker/src/main.rs:66-74`; parse `:129` |
| `--control` / `CONTROL_URL` | CLI+env | URL | `http://localhost:9090` | no | no | Control-plane base URL (version/env poll) | `crates/worker/src/main.rs:75` |
| `--control-key` / `CONTROL_KEY` | CLI+env | string | `""` | **yes** | **yes** (unless `--dev-insecure`) | Bearer key for pulling versions/env from control | `crates/worker/src/main.rs:76`; validate `:25-30`, `:91-94` |
| `--dev-insecure` / `ZEROSHIP_DEV_INSECURE` | CLI+env | bool (`1` only for env) | `false` | no | n/a | Relaxes control-key guard | `crates/worker/src/main.rs:18-23`, `:77` |
| `--max-isolates` / `MAX_ISOLATES` | CLI+env | usize (string) | `200` | no | no | LRU cap on V8 isolates (`.parse().unwrap_or(200)`) | `crates/worker/src/main.rs:78`; parse `:121` |
| `--poll-interval` / `POLL_INTERVAL` | CLI+env | u64 secs (string) | `5` | no | no | Version/env poll cadence (`.parse().unwrap_or(5)`) | `crates/worker/src/main.rs:79`; parse `:122` |
| `--db` / `DATABASE_URL` | CLI+env | string (DSN) | `""` → `None` | no (DSN may embed creds) | no | Per-app DB plugin connection (empty → `db_url: None`) | `crates/worker/src/main.rs:80`; map `:120` |
| `--worker-key` / `WORKER_KEY` | CLI+env | string | `""` | **yes** | conditional: **yes if non-loopback bind**, else warns | `/dispatch` bearer + `ZeroShip-User` HMAC verification | `crates/worker/src/main.rs:81`; guard `:96-109` |
| `--shutdown-timeout` / `SHUTDOWN_TIMEOUT` | CLI+env | u64 secs (string) | `30` | no | no | SIGTERM drain deadline (`.parse().unwrap_or(30)`; `0`=forever) | `crates/worker/src/main.rs:82`; parse `:124`; apply `:165-166`,`:198` |
| `--blob-store` / `BLOB_STORE` | CLI+env | path | `./bundles` | no | no (panics if init fails) | Content-addressed blob store root | `crates/worker/src/main.rs:85`; init `:111-114` |
| `--bind` / `WORKER_BIND` | CLI+env | host/IP | `127.0.0.1` | no | no (but gates the WORKER_KEY requirement) | Bind host; non-loopback requires WORKER_KEY | `crates/worker/src/main.rs:89`; guard `:96-109` |
| `--socket` / `WORKER_SOCKET` | CLI+env | path | `""` (disabled) | no | no | Optional extra Unix-domain-socket bind | `crates/worker/src/main.rs:128`; bind `:150-154`,`:202-204` |

### Config files

None of the three binaries reads a structured config file (no TOML/YAML/JSON/dotenv loader). The only file *paths* they consume are individual key/secret files, all gated by the flags above:

- **control**: `SIGNING_KEY_FILE` (PAT signing key, loaded via `token_handlers::load_signing_key_from_path`, `crates/control/src/main.rs:227`); `BUILDER_CLIENT_SECRET_FILE` (default `data/builder-client-secret`, `crates/control/src/main.rs:127-132`); `DEPLOY_TMP_DIR` (scratch dir, write-probed at boot, `:150-171`).
- **gateway**: `GATEWAY_SIGNING_KEY_FILE` (Ed25519 PKCS#8 PEM/DER wrapper-token key, `signing::load_from_path`, `crates/gateway/src/main.rs:144`).
- **worker**: none (only the `WORKER_SOCKET` UDS path, which is created/bound, not read).
- Referenced-but-not-read-by-these-binaries: `ops/auth-clients.example.toml` is mentioned in comments (`crates/control/src/main.rs:543`, `crates/gateway/src/main.rs:247,326`) as the source of truth for OIDC client registration, but it is consumed by Hydra/ops tooling, not parsed by any of these three `main.rs` files.

### Notes

**Hydra-admin double-name drift (control only).** Control accepts the hydra-admin URL under **two different env names**: `AUTH_HYDRA_ADMIN` (flag `--hydra-admin`, read into `legacy_hydra_admin_url`, `crates/control/src/main.rs:281-286`) and `HYDRA_ADMIN_URL` (flag `--hydra-admin-url`, `:297`). The fallback at `:296-303` prefers `HYDRA_ADMIN_URL`/`--hydra-admin-url`; only if that is empty does it fall back to `AUTH_HYDRA_ADMIN`/`--hydra-admin`. The boot guard (`:325`) checks the *resolved* value. So `AUTH_HYDRA_ADMIN` is a pure legacy alias kept alive solely by glue logic — a prime unification target.

**Dev-insecure: three spellings, inconsistent across binaries.**
- control: `--dev-insecure` or `ZEROSHIP_DEV_INSECURE=1` (`:102-104`).
- worker: same two (`--dev-insecure` / `ZEROSHIP_DEV_INSECURE=1`, `:18-23`).
- gateway: those two **plus** a third pair `--insecure-dev` / `INSECURE_DEV=true` (`crates/gateway/src/main.rs:20-26`). Env-value semantics also differ: `ZEROSHIP_DEV_INSECURE` matches only `"1"`, while `INSECURE_DEV` matches `"true"` (case-insensitive). control's `flag_or_env` helper accepts `"1"` *or* `"true"`, but the inline dev-insecure/trust-proxy checks accept only `"1"` — inconsistent truthy parsing even **within** control.

**`CONTROL_KEY` semantics differ by binary.** Same name everywhere, but: control *issues/validates* it for inbound admin+internal auth and **requires** it (non-dev); gateway and worker *present* it outbound to control and also **require** it (non-dev). All three default to `""` and gate on `insecure_dev`.

**`WORKER_KEY` enforcement differs by binary.** Default `""` in all three. control: read, no guard (silently unauthenticated). gateway: warns only (`:128-132`). worker: **conditionally fatal** — empty allowed only on a loopback bind; a non-loopback bind with empty `WORKER_KEY` exits(1) (`:96-109`).

**`--workers` is overloaded.** control/gateway: `--workers`/`WORKER_URLS` = CSV of worker URLs (default `http://localhost:8080`). worker: `--workers`/`WORKER_THREADS` = ntex thread count (`crates/worker/src/main.rs:66`). Same flag, completely different meaning. Sharp drift.

**`DATABASE_URL` default differs across binaries.** control → `postgres://localhost/zeroship` (live default); gateway/worker → `""` (degraded mode: gateway disables session validation, worker sets `db_url: None`). Same env name, three different empty/non-empty semantics.

**Blob-store roots — name drift for the same concept.** control: `--bundles`/`BUNDLES_DIR` (default `./bundles`); gateway/worker: `--blob-store`/`BLOB_STORE` (default `./bundles`). Identical default value, **different flag+env name**. Worker comment (`:83-84`) explicitly notes the default was chosen to match, yet the name was not unified.

**`STASH_SIGNING_KEY` shared by control + gateway, same dev sentinel, different validation.** Both fall back to literal `dev-stash-key-please-rotate`. **gateway validates strength** (rejects dev-default + <32 B); **control only checks presence** (empty vs non-empty). Same secret, stricter rules in one binary than the other. worker does not use a stash key.

**`AUTH_PUBLIC` shared by control + gateway, different defaults + roles.** control default `""` (dev fallback `http://localhost:4444`) and **requires** it non-dev; gateway default `http://auth:9092` and **never requires** it.

**Insecure built-in default ships in gateway: `GATEWAY_OIDC_SECRET`.** Defaults to literal `dev-secret-rotate-me-too` (`:84-89`), **never** validated or required — a production gateway with the env unset silently runs on a known secret. Asymmetry with control's `CONSOLE_OIDC_SECRET` (required non-dev). Security smell.

**Numeric parses silently swallow bad input.** Every numeric config uses `.parse().unwrap_or(<default>)` — a typo'd value (e.g. `MAX_ISOLATES=abc`) silently reverts to default rather than erroring.

**Boot-validation styles diverge.** control hand-rolls a large inline two-phase missing-secrets block plus master-key strength decode and a `DEPLOY_TMP_DIR` write-probe; gateway/worker use small named `validate_*` helpers + `process::exit(1)`. No shared validation module. Each binary re-implements `arg_or_env` verbatim (control `:24-31`, gateway `:342-349`, worker `:216-223`); control additionally has `env_or` and `flag_or_env`.

**Config-shaped values that are hard-coded (not externalized).** gateway: `RateLimitRegistry::new(1000, 2000)` (`:279`), `ConcurrencyRegistry::new(100)` (`:281`), CHWBL `max_per_worker = 500`, `vnodes = 150` (`:211`, `:220`); control: admin/webhook quotas `Quota::per_minute(30,60)` / `(50,600)` (`:438-439`); literal OIDC client_ids + audience strings. None flag/env-driven today.

**Completeness:** control 23, gateway 21, worker 13 distinct inputs; no rows skipped. Every `arg_or_env`/`flag_or_env`/`env_or`/`std::env::var` hit in the three files accounted for.
## Auth server (crates/auth)

All non-test config is a single clap `#[derive(Parser)]` struct `AuthConfig` (`crates/auth/src/config.rs`). Every field is `CLI+env`: long flag (kebab-cased field name) + an `AUTH_*` env var. Parsed in `main.rs:30` via `AuthConfig::parse()`.

| Name | Kind | Type | Default | Secret? | Refuses boot if unset? | Controls | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `addr` / `AUTH_ADDR` | CLI+env | String | `0.0.0.0:9092` | no | no | Listen/bind address | config.rs:9 |
| `db_url` / `AUTH_DB_URL` | CLI+env | String | **required** (no default) | yes (DSN w/ creds) | **yes** — clap errors out if neither flag nor env set | PostgreSQL DSN | config.rs:13 |
| `hydra_admin` / `AUTH_HYDRA_ADMIN` | CLI+env | String | `http://127.0.0.1:4445` | no | conditionally — non-loopback host aborts boot unless `allow_remote_hydra_admin=true` | Hydra admin API base URL (loopback) | config.rs:17 |
| `allow_remote_hydra_admin` / `AUTH_ALLOW_REMOTE_HYDRA_ADMIN` | CLI+env | bool | `false` (bare flag ⇒ `true`; `num_args=0..=1`) | no | no | Permit non-loopback `hydra_admin`; otherwise boot aborts | config.rs:23-31 |
| `hydra_public` / `AUTH_HYDRA_PUBLIC` | CLI+env | String | `https://auth.zeroship.ai` | no | no | Hydra public base URL (OIDC issuer) | config.rs:34 |
| `clients_config` / `AUTH_CLIENTS_CONFIG` | CLI+env | String (path) | `/etc/zeroship/auth-clients.toml` | no | conditionally — `bootstrap::run` reads this TOML; unreadable/unparseable ⇒ fatal `AuthError::Bootstrap` (main.rs:71) | Path to OIDC clients TOML | config.rs:38 |
| `bootstrap` / `AUTH_BOOTSTRAP` | CLI+env | bool | `false` | no | indirectly — if `false` AND hydra JWK sets empty ⇒ fatal | Allow first-boot JWK + client creation | config.rs:43 |
| `insecure_dev` / `AUTH_INSECURE_DEV` | CLI+env | bool | `false` | no | no | Dev mode: drop Secure flag on cookies; also relaxes stash-key validation | config.rs:47 |
| `stash_signing_key` / `AUTH_STASH_SIGNING_KEY` | CLI+env | String | `dev-only-stash-signing-key-not-for-production-use!!` | **yes** | **yes (prod)** — dev-default or `<32` bytes ⇒ `process::exit(1)` unless `insecure_dev` | HMAC key signing the federation stash cookie | config.rs:56-61 |
| `google_client_id` / `AUTH_GOOGLE_CLIENT_ID` | CLI+env | Option\<String\> | `None` | no (ID) | no — if unset, `/oauth/google/*` routes unregistered, warn logged | Google OAuth client ID (enables Google federation) | config.rs:66 |
| `google_client_secret` / `AUTH_GOOGLE_CLIENT_SECRET` | CLI+env | Option\<String\> | `None` | **yes** | no | Google OAuth client secret | config.rs:70 |
| `google_redirect_uri` / `AUTH_GOOGLE_REDIRECT_URI` | CLI+env | String | `https://auth.zeroship.ai/oauth/google/callback` | no | no | Google redirect URI (must match Google console) | config.rs:75-80 |
| `google_auth_url` / `AUTH_GOOGLE_AUTH_URL` | CLI+env | String | `https://accounts.google.com/o/oauth2/v2/auth` | no | no | Google authorize endpoint (overridable for e2e mock) | config.rs:85-90 |
| `google_token_url` / `AUTH_GOOGLE_TOKEN_URL` | CLI+env | String | `https://oauth2.googleapis.com/token` | no | no | Google token endpoint (overridable for tests) | config.rs:94-99 |
| `google_jwks_url` / `AUTH_GOOGLE_JWKS_URL` | CLI+env | String | `https://www.googleapis.com/oauth2/v3/certs` | no | no | Google JWKS endpoint (overridable for tests) | config.rs:103-108 |
| `google_issuer` / `AUTH_GOOGLE_ISSUER` | CLI+env | String | `https://accounts.google.com` | no | no | Expected `iss` on Google ID tokens (overridable for tests) | config.rs:114-119 |
| `github_client_id` / `AUTH_GITHUB_CLIENT_ID` | CLI+env | Option\<String\> | `None` | no (ID) | no — if unset, `/oauth/github/*` routes unregistered, warn logged | GitHub OAuth client ID (enables GitHub federation) | config.rs:124 |
| `github_client_secret` / `AUTH_GITHUB_CLIENT_SECRET` | CLI+env | Option\<String\> | `None` | **yes** | no | GitHub OAuth client secret | config.rs:128 |
| `github_redirect_uri` / `AUTH_GITHUB_REDIRECT_URI` | CLI+env | String | `https://auth.zeroship.ai/oauth/github/callback` | no | no | GitHub callback URL | config.rs:133-138 |
| `github_authorize_url` / `AUTH_GITHUB_AUTHORIZE_URL` | CLI+env | String | `https://github.com/login/oauth/authorize` | no | no | GitHub authorize endpoint (overridable for tests) | config.rs:142-147 |
| `github_token_url` / `AUTH_GITHUB_TOKEN_URL` | CLI+env | String | `https://github.com/login/oauth/access_token` | no | no | GitHub token endpoint (overridable for tests) | config.rs:150-155 |
| `github_user_url` / `AUTH_GITHUB_USER_URL` | CLI+env | String | `https://api.github.com/user` | no | no | GitHub `/user` endpoint (overridable for tests) | config.rs:158-163 |
| `github_emails_url` / `AUTH_GITHUB_EMAILS_URL` | CLI+env | String | `https://api.github.com/user/emails` | no | no | GitHub `/user/emails` endpoint (overridable for tests) | config.rs:166-171 |
| `mailer` / `AUTH_MAILER` | CLI+env | String | `stdout` | no | no | Mailer driver select: `stdout`\|`smtp`\|`resend` | config.rs:175 |
| `smtp_host` / `AUTH_SMTP_HOST` | CLI+env | Option\<String\> | `None` | no | no (but required when `mailer=smtp`) | SMTP relay hostname | config.rs:179 |
| `smtp_port` / `AUTH_SMTP_PORT` | CLI+env | u16 | `587` | no | no | SMTP port (587 STARTTLS / 465 SMTPS) | config.rs:183 |
| `smtp_username` / `AUTH_SMTP_USERNAME` | CLI+env | Option\<String\> | `None` | no | no | SMTP username (optional) | config.rs:187 |
| `smtp_password` / `AUTH_SMTP_PASSWORD` | CLI+env | Option\<String\> | `None` | **yes** | no | SMTP password | config.rs:191 |
| `smtp_starttls` / `AUTH_SMTP_STARTTLS` | CLI+env | bool | `true` | no | no | STARTTLS (true) vs implicit TLS/SMTPS (false) | config.rs:196 |
| `resend_api_key` / `AUTH_RESEND_API_KEY` | CLI+env | Option\<String\> | `None` | **yes** | no (but required when `mailer=resend`) | Resend API key | config.rs:200 |
| `mail_from_email` / `AUTH_MAIL_FROM_EMAIL` | CLI+env | String | `auth@zeroship.ai` | no | no | `From` address on transactional mail | config.rs:204-209 |
| `mail_from_name` / `AUTH_MAIL_FROM_NAME` | CLI+env | String | `zeroship` | no | no | `From` display name | config.rs:212 |
| `public_url` / `AUTH_PUBLIC_URL` | CLI+env | String | `http://localhost:9092` | no | no | Public origin for absolute URLs in email (magic links); trailing `/` trimmed by `public_url()` | config.rs:222-227 |
| `postmark_webhook_user` / `AUTH_POSTMARK_WEBHOOK_USER` | CLI+env | Option\<String\> | `None` | no | no — when unset, `POST /webhooks/postmark` returns 401 | Postmark webhook HTTP Basic username | config.rs:235 |
| `postmark_webhook_password` / `AUTH_POSTMARK_WEBHOOK_PASSWORD` | CLI+env | Option\<String\> | `None` | **yes** | no | Postmark webhook HTTP Basic password | config.rs:240 |
| `jwk_rotation_days` / `AUTH_JWK_ROTATION_DAYS` | CLI+env | i64 | `90` | no | no | Days between JWK rotations | config.rs:248 |
| `jwk_retain_days` / `AUTH_JWK_RETAIN_DAYS` | CLI+env | i64 | `31` | no | no | Days to retain outgoing keys past rotation | config.rs:256 |
| `cron_tick_secs` / `AUTH_CRON_TICK_SECS` | CLI+env | u64 | `86400` | no | no | Cron tick interval (sec); lowered for tests | config.rs:262 |
| `audit_retention_check_secs` / `AUTH_AUDIT_RETENTION_CHECK_SECS` | CLI+env | u64 | `3600` | no | no | Audit-retention sweeper tick interval (sec) | config.rs:270-275 |

All 40 fields of `AuthConfig`; no rows skipped.

### Config files

**OIDC clients TOML** — path from `clients_config` / `AUTH_CLIENTS_CONFIG` (default `/etc/zeroship/auth-clients.toml`). Loaded by `ClientsConfig::from_path` (`crates/auth/src/bootstrap/clients_config.rs:65`), parsed with `toml::from_str`, reconciled against Hydra admin (upsert, never deletes) at every boot. Example: `ops/auth-clients.example.toml`.

Schema — top-level is an array of `[[client]]` tables (`#[serde(rename = "client")]`), deserialized into `ClientEntry` (clients_config.rs:30-51):

| TOML key | Type | Required? | Default | Notes |
| --- | --- | --- | --- | --- |
| `client_id` | String | **yes** | — | OAuth client ID |
| `client_name` | String | no | `None` | Display name |
| `client_secret` | String | no | `None` | Auto-generated if absent for confidential clients (secret) |
| `grant_types` | [String] | no | `["authorization_code","refresh_token"]` | `default_grant_types()` |
| `response_types` | [String] | no | `["code"]` | `default_response_types()` |
| `redirect_uris` | [String] | no | `[]` | |
| `post_logout_redirect_uris` | [String] | no | `[]` | |
| `scope` | String | **yes** | — | Space-delimited scopes |
| `token_endpoint_auth_method` | String | no | `client_secret_basic` | `default_auth_method()` |
| `subject_type` | String | no | `public` | `default_subject_type()` |
| `access_token_strategy` | String | no | `None` | e.g. `jwt` |
| `id_token_signed_response_alg` | String | no | `None` | e.g. `EdDSA` |
| `audience` | [String] | no | `[]` | |
| `first_party` | bool | no | `false` | true ⇒ `skip_consent=true`, `require_consent=false` (clients_config.rs:92-94) |
| `frontchannel_logout_uri` | String | no | `None` | OIDC front-channel logout |
| `backchannel_logout_uri` | String | no | `None` | OIDC back-channel logout |

`require_logout_consent` is always hardcoded `false`. Example file defines three first-party clients: `console.zeroship.ai`, `zeroship-cli` (device-code grant, public client), and `gateway` (hosted apps; redirect_uris appended per-deploy by control plane). No other config-file formats in scope (no JSON/YAML/`.env` loader; clap reads the process environment directly).

### Notes

**Boot-refusing validation** (all in `main.rs:30-72`, run after parse):
1. `db_url` has no clap default ⇒ clap errors and exits if neither `--db-url` nor `AUTH_DB_URL` provided. Only hard-required input.
2. `validate_stash_key` (config.rs:288-313) → `process::exit(1)`: if `insecure_dev` always Ok (warns on dev-prefix); else dev-default prefix ⇒ refuse; else `<32` bytes ⇒ refuse. Shipped default is fatal in prod.
3. `validate_hydra_admin_url` (startup_validation.rs:7-26), main.rs:37 → `process::exit(1)`: non-loopback `hydra_admin` host (DNS-resolved; all addrs must be loopback) with `allow_remote_hydra_admin=false` ⇒ refuse. Flag downgrades to warning. (Not in original task list — a real config-gated boot refusal.)
4. `bootstrap::run`: `bootstrap=false` AND a Hydra JWK set empty ⇒ fatal `AuthError::Bootstrap`. State-dependent required toggle.

**Non-fatal gating:** missing google/github client_id ⇒ warn + routes unregistered; `mailer=smtp` needs `smtp_host`, `mailer=resend` needs `resend_api_key` (driver-construction-time); Postmark webhook 401 when creds unset.

**Test-only env vars** (NOT read by the server binary; in `crates/auth/tests/` or `#[cfg(test)]`):
- `AUTH_DB_URL` — skip-if-unset guard in most integration tests (same name as prod var).
- `AUTH_HYDRA_ADMIN` — test skip-guard for hydra e2e (same name as prod var).
- `AUTH_HYDRA_PUBLIC_URL` — **test-only, distinct name** (`_URL` suffix, unlike prod `AUTH_HYDRA_PUBLIC`). Name-collision trap.
- `AUTH_LOAD_TEST` — gates `tests/load_test.rs`.
- `PG_TEST_URL` — alternate DSN fallback in `tests/m4_post_redeem_test.rs`.

**Surprises:** `addr` (bind, `0.0.0.0:9092`) vs `public_url` (origin, `http://localhost:9092`) deliberately separate. `AUTH_HYDRA_PUBLIC` (server/issuer) vs `AUTH_HYDRA_PUBLIC_URL` (test-only) is an easy collision trap. Many Google/GitHub endpoint URLs are override-for-tests only. No compile-time config in scope.
## Sandbox controller + in-VM agent (crates/sandbox, crates/sandbox-agent)

Sandbox is **env-only — no CLI flags, no config file** (the binaries `zeroship-sandbox` / `sandbox-agent` take no argv config; everything is read from env, mostly at boot). All vars read via `std::env::var` (no clap/argv). `parse_env(key, default)` is the typed helper in `config.rs:1035`; `db.rs` has its own `parse_env_{i64,u64,usize}`; `sweep.rs` has `read_{u64,i64,usize}_env`.

| Name | Kind | Type | Default | Secret? | Refuses boot if unset? | Controls | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `SANDBOX_PORT` | env | u16 | `9091` | no | no | HTTP API listen port | config.rs:847 |
| `SANDBOX_TOKEN` | env | String (→`ApiToken`) | `""` | **yes** | **yes** — empty refused unless `SANDBOX_ALLOW_NO_AUTH=true`; non-empty `&lt;32` B always refused | Creator-side bearer for `/sandbox/*` auth | config.rs:848,855,863 |
| `SANDBOX_ALLOW_NO_AUTH` | env | bool | `false` | no | n/a (allows empty-token boot) | Dev opt-in to run with auth disabled | config.rs:854 |
| `SANDBOX_BACKEND` | env | enum {docker,k8s,nomad-ch} | `docker` | no | **yes** if not in set | Backend selector | config.rs:871 |
| `SANDBOX_IMAGE` | env | String | `zeroship/sandbox-base:latest` | no | no | Docker image tag | config.rs:877 |
| `SANDBOX_WORKSPACE_ROOT` | env | PathBuf | `/var/zeroship/projects` | no | no | Host workspace bind-mount root | config.rs:879 |
| `SANDBOX_NETWORK` | env | String | `zeroship-sandbox-net` | no | no | Docker network name | config.rs:883 |
| `SANDBOX_MEMORY_MB` | env | u32 | `1024` | no | **yes** if `&lt;64` | Per-container memory limit (MiB) | config.rs:885,894 |
| `SANDBOX_CPUS` | env | f32 | `2.0` | no | **yes** if `&lt;=0` or `&gt;64` | Per-container CPU quota | config.rs:886,891 |
| `SANDBOX_IDLE_TIMEOUT_SECS` | env | u64 | `1800` | no | no | Idle session GC threshold | config.rs:887 |
| `SANDBOX_MAX_LIFETIME_SECS` | env | u64 | `28800` | no | no | Hard session lifetime ceiling (8h) | config.rs:888 |
| `SANDBOX_AUTO_PULL` | env | bool | `false` | no | no | Pull image at startup if missing | config.rs:889 |
| `SANDBOX_K8S_NAMESPACE` | env | String | `default` | no | no | k8s Pod namespace | config.rs:902 |
| `SANDBOX_K8S_IMAGE` | env | String | `docker.io/zeroship/sandbox-agent:dev` | no | no | k8s agent OCI image | config.rs:904 |
| `SANDBOX_K8S_RUNTIME_CLASS` | env | String | `kvm-sandbox` | no | no | k8s `runtimeClassName` | config.rs:906 |
| `SANDBOX_K8S_READY_TIMEOUT_SECS` | env | u64 | `120` | no | no | `kubectl wait` Ready timeout | config.rs:908 |
| `SANDBOX_K8S_USE_PORT_FORWARD` | env | bool | `true` | no | no | port-forward vs direct Pod IP | config.rs:909 |
| `SANDBOX_K8S_PORT_FORWARD_START` | env | u16 | `18000` | no | no | Loopback port-allocator base | config.rs:910 |
| `SANDBOX_K8S_USER_HOME_SIZE` | env | String (Quantity) | `5Gi` | no | no | Per-user `/home/u` PVC size | config.rs:911 |
| `SANDBOX_K8S_USER_HOME_STORAGE_CLASS` | env | Option\<String\> | `None` (empty→cluster default) | no | no | StorageClass for per-user PVCs | config.rs:898 |
| `SANDBOX_K8S_STARTUP_ORPHAN_CLEANUP` | env | bool | `false` | no | no | Delete orphan agent Pods at boot (dangerous in HA) | config.rs:914 |
| `SANDBOX_NOMAD_ADDR` | env | String (URL) | `http://127.0.0.1:4646` | no | **yes** — must be `http(s)://` AND loopback host | Nomad HTTP API base URL | config.rs:918 |
| `SANDBOX_NOMAD_DATACENTER` | env | String | `dc1` | no | no | Nomad jobspec datacenter | config.rs:920 |
| `SANDBOX_NOMAD_CH_RUNTIME_DIR` | env | PathBuf | `/var/lib/zeroship/ch` | no | no | Kernel/rootfs template dir (→`ZSBX_ARTIFACT_DIR`) | config.rs:922 |
| `SANDBOX_NOMAD_CH_HOST_STATE_DIR` | env | PathBuf | `/var/zeroship/ch` | no | no | Per-sandbox host state root | config.rs:926 |
| `SANDBOX_NOMAD_CH_USER_HOME_ROOT` | env | PathBuf | `/var/zeroship/ch/users` | no | **yes** if non-canonical descendant of host_state_dir | Per-user persistent home root | config.rs:930 |
| `SANDBOX_NOMAD_CH_VM_INDEX_FLOOR` | env | u16 | `1` | no | **yes** if `&lt;1` or `&gt;ceil` | Lower bound of per-VM index pool | config.rs:934 |
| `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` | env | u16 | `20` | no | **yes** if `100+ceil&gt;255` (`&gt;155`) | Upper bound of index pool | config.rs:935 |
| `SANDBOX_NOMAD_CH_ALLOC_RUNNING_TIMEOUT_SECS` | env | u64 | `120` | no | no | Wait for alloc `running` | config.rs:936 |
| `SANDBOX_NOMAD_CH_AGENT_LIVEZ_TIMEOUT_SECS` | env | u64 | `30` | no | no | Wait for in-VM agent `/livez` | config.rs:940 |
| `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS` | env | u64 | `120` | no | no | FM-F host fence before vm_index release; `0` disables | config.rs:944 |
| `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP` | env | bool | `false` | no | no | Stop+purge `zsbx-` jobs at boot (dangerous in HA) | config.rs:948 |
| `SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET` | env | u8 | `99` | no | **yes** if `127`/`169`/`224..=255` | Second octet of per-VM /30 subnet | config.rs:952 |
| `SANDBOX_NOMAD_CH_VM_INDEX_RELEASE_DELAY_SECS` | env | u64 | `2` | no | no | Delay after stop-ACK before vm_index release; `0` disables | config.rs:956 |
| `SANDBOX_NOMAD_STOP_CONCURRENCY` | env | usize | `16` | no | **yes** if `0` (deadlocks teardown) | Global cap on in-flight Nomad `/shutdown` | config.rs:963; validate :725 |
| `SANDBOX_CREATE_RETRY_MAX` | env | u32 | `2` | no | no | Extra `backend.create()` retries | config.rs:972 |
| `SANDBOX_CREATE_RETRY_TOTAL_TIMEOUT_SECS` | env | u64 | `90` | no | no | Wall-time budget for create+retry chain | config.rs:973 |
| `SANDBOX_SNAPSHOT_ENABLED` | env | bool | `false` | no | no (gates other guards) | Snapshot/restore master feature flag | config.rs:975 |
| `SANDBOX_SNAPSHOT_L1_ROOT` | env | PathBuf | `/var/zeroship/ch/snapshots` | no | no | L1 snapshot artifact root | config.rs:976 |
| `SANDBOX_SNAPSHOT_USE_GCS` | env | bool | `false` | no | **yes** if true+enabled+no bucket; or true+enabled+no KEK | Wrap L1 in tiered L1/L2-GCS | config.rs:980 |
| `SANDBOX_SNAPSHOT_GCS_BUCKET` | env | Option\<String\> | `None` | no | conditionally (see use_gcs) | GCS bucket (no `gs://`) | config.rs:981 |
| `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` | env | Option\<PathBuf\> (file =32 B) | `None` (→AEAD passthrough) | **yes** (file is the key) | conditionally — required for remote GCS unless test override | Root KEK for snapshot AEAD-at-rest | config.rs:984; snapshot_aead.rs:156,223 |
| `SANDBOX_WORKSPACE_IMAGE_SIZE_GB` | env | u32 | `20` | no | **yes** if `0` | workspace.img + home.img size | config.rs:1001,1003 |
| `SANDBOX_DRIVER_STAGES_DISK_IMAGES` | env | bool | `false` | no | no | Option-C: driver stages disk images | config.rs:1015 |
| `SANDBOX_WAKE_RESPONSE_MODE` | env | enum {sync,async,""} | `sync` | no | **yes** — fail-CLOSED on any other value | Wake-response contract (200 sync vs 202+poll) | config.rs:1085 |
| `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS` | env | u64 | `300` | no | **yes** if `&lt;1` | Retention for terminal wake_jobs rows | config.rs:1168 |
| `SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS` | env | u64 | `60` | no | **yes** if `&lt;30` | Staleness threshold for wake_jobs takeover | config.rs:1188 |
| `SANDBOX_DATABASE_URL` | env | String (DSN) | unset → **pg disabled** (`Ok(None)`) | **yes** | **yes** if set but bad scheme | Primary `sandbox_app` DSN | db.rs:432; scheme :1015 |
| `SANDBOX_DATABASE_URL_AUDIT` | env | String (DSN) | falls back to `SANDBOX_DATABASE_URL` | **yes** | no (validated if set) | `sandbox_audit` role DSN | db.rs:489 |
| `SANDBOX_DATABASE_URL_GDPR` | env | String (DSN) | falls back to `SANDBOX_DATABASE_URL` | **yes** | no (validated if set) | `sandbox_gdpr` role DSN | db.rs:490 |
| `SANDBOX_DATABASE_PASSWORD_PATH` | env | PathBuf | unset (no injection) | **yes** (file is the pw) | **yes** if set but mode≠0400 / wrong owner / unreadable | Reads pw file, injects into DSN(s) | db.rs:1032; checks :958-971 |
| `SANDBOX_PG_RUN_MIGRATIONS` | env | bool (`=="1"`) | `0`/unset | no | no | Tags this replica as designated migrator | db.rs:457 |
| `SANDBOX_PG_BOOT_TIMEOUT_SECS` | env | u64 | `60` | no | no | Schema-converge boot timeout | db.rs:461 |
| `SANDBOX_PG_POOL_MAX` | env | usize | `16` | no | **yes** if `0` | Pg pool max size | db.rs:462,463 |
| `SANDBOX_PG_OPTIONAL` | env | bool (`=="1"`) | `0`/unset | no | no (prevents boot-fail) | Dev escape hatch: tolerate pg failure | lib.rs:724,745 |
| `SANDBOX_HA_HEARTBEAT_SECS` | env | i64 | `5` | no | **yes** if `&lt;=0` | HA heartbeat interval | db.rs:1133 |
| `SANDBOX_HA_LEASE_TTL_SECS` | env | i64 | `60` | no | **yes** if `&lt;=0` or `&lt; 4×heartbeat` | HA lease TTL | db.rs:1134,1152 |
| `SANDBOX_HA_AUTO_TAKEOVER` | env | bool (`=="1"`) | `0`/unset | no | no | Enable dead-host takeover (needs pg) | lib.rs:1255 |
| `SANDBOX_HA_DRAIN_GRACE_SECS` | env | u64 | `30` | no | no | Post-SIGTERM background-task drain wait | main.rs:303 |
| `SANDBOX_HOST_ID` | env | String (typed-id or UUID) | unset → file → fresh UUIDv7 | no | **yes** if set but unparseable | Stable controller identity | db.rs:1198,1212 |
| `SANDBOX_HOSTNAME` | env | String | unset → `/proc/.../hostname` → `unknown` | no | no | Hostname for `hosts` row | lib.rs:1642 |
| `SANDBOX_REGION` | env | String | `us-local-1` | no | no | Region label in `upsert_host` | db.rs:1874 |
| `SANDBOX_PERSIST_AUTH` | env | bool (`=="1"`) | `0`/unset → persistence `None` | no | no (interacts w/ snapshot guard) | Enable sealed-record persistence | persist.rs:623; gate lib.rs ~681 |
| `SANDBOX_PERSIST_DIR` | env | PathBuf | `/var/lib/zeroship/sandbox` | no | no | Parent dir for sealed-records/state | persist.rs:631; db.rs:1253 |
| `SANDBOX_AEAD_KEY_PATH` | env | PathBuf (file-mount only) | unset | **yes** (file is the key) | **yes** if `SANDBOX_PERSIST_AUTH=1` and unset | Persistence AEAD key | persist.rs:626 |
| `SANDBOX_ADMIN_TOKEN_PATH` | env | PathBuf | unset → admin API disabled | **yes** (file is bearer) | **yes** if file present-but-misconfigured (mode≠0400/owner≠0/empty); or equals RO token | `sandbox_admin` Full bearer file | lib.rs:947 |
| `SANDBOX_ADMIN_RO_TOKEN_PATH` | env | PathBuf | unset → RO admin disabled | **yes** (file is bearer) | **yes** same checks; refuses if identical to Full | `sandbox_admin` read-only bearer | lib.rs:959 |
| `SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS` | env | i64 | `120` | no | no (silent default) | §6.1 transient-state takeover threshold | sweep.rs:259 |
| `SANDBOX_IDLE_SNAPSHOT_SWEEP_SECS` | env | u64 | `300` (floored `.max(1)`) | no | no | Idle-eviction sweep cadence | sweep.rs:788 |
| `SANDBOX_IDLE_SNAPSHOT_SECS` | env | i64 | `1800`; `&lt;=0` disables | no | no | Idle threshold for snapshot eviction | sweep.rs:793 |
| `SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY` | env | usize | `2` (floored `.max(1)`) | no | no | Per-chunk cap on snapshot ops | sweep.rs:797 |
| `SANDBOX_HOST_DIR_GC_POLL_SECS` | env | u64 | `300` (floored `.max(60)`) | no | no | host_dir GC sweep cadence | sweep.rs:1253 |
| `SANDBOX_HOST_DIR_GC_GRACE_SECS` | env | u64 | `600` (floored `.max(60)`) | no | no | Min mtime age before reaping host_dir | sweep.rs:1255 |
| `SANDBOX_PREVIEW_MAX_BODY_BYTES` | env | usize | `104857600` (100 MiB) | no | no | Controller preview-proxy body cap | preview.rs:390 |
| `SANDBOX_PREVIEW_WS_PORT` | env | u16 | `9092` | no | no | Controller preview WS-Upgrade port | preview_ws.rs:80 |
| `ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION` | env | bool (`=="1"`) | unset | no | no (bypasses a boot guard) | **TEST-ONLY**: skip persist-required-when-snapshot assert | lib.rs:682 |
| `ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE` | env | bool (`=="1"`) | unset | no | no (bypasses a boot guard) | **TEST-ONLY**: allow GCS L2 without root KEK | lib.rs:704 |

### In-VM agent (`crates/sandbox-agent`)

| Name | Kind | Type | Default | Secret? | Refuses boot if unset? | Controls | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `SANDBOX_AGENT_PORT` | env | u16 | `7777` | no | **yes** if set but unparseable | Agent HTTP listen port | sandbox-agent/src/main.rs:57; lib.rs:39 |
| `SANDBOX_AGENT_WS_PORT` | env | u16 | `7778` (HTTP+1) | no | no | Agent WS-Upgrade listen port | sandbox-agent/src/proxy_ws.rs:98 |
| `SANDBOX_AGENT_WORKSPACE` | env | PathBuf | `/workspace` | no | no | Workspace dir bound by the agent | sandbox-agent/src/main.rs:54; lib.rs:35 |
| `SANDBOX_AGENT_SANDBOX_ID` | env | String (UUID) | unset → `/run/keys/sandbox-id` | no | **yes** — aborts if neither env nor file | Agent boot sandbox_id (for `/_clock_resync` sig match) | sandbox-agent/src/handlers.rs:139; lib.rs:81; main.rs:97 |
| `SANDBOX_AGENT_PUBKEY_FILE` | env | PathBuf | `DEFAULT_PUBKEY_PATH` (`/run/keys/controller-pubkey`) | no (pubkey) | **yes** if file missing/unreadable | Controller ED25519 pubkey for request verification | sandbox-agent/src/lib.rs:76 |
| `SANDBOX_AGENT_LOG` | env | String (filter) | `info,sandbox_agent=debug` | no | no | Log filter (only if `RUST_LOG` unset) | sandbox-agent/src/main.rs:45 |
| `SANDBOX_AGENT_PROXY_MAX_BODY_BYTES` | env | usize | `104857600` (100 MiB) | no | no | Agent proxy body cap | sandbox-agent/src/proxy.rs:271,60 |
| `RUST_LOG` | env | String (filter) | — (falls back to `SANDBOX_AGENT_LOG`) | no | no | Standard tracing filter; takes precedence | sandbox-agent/src/main.rs:40 |

### Config files

No config-file format (no TOML/YAML/JSON) consumed by either crate — configuration is 100% env. File *paths* pointed to by env vars (not config files): secret/key files (`SANDBOX_DATABASE_PASSWORD_PATH` mode-0400 owner-checked, `SANDBOX_AEAD_KEY_PATH`, `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` 32 raw bytes, `SANDBOX_ADMIN_TOKEN_PATH`/`_RO` mode-0o400 owner-uid-0, `SANDBOX_AGENT_PUBKEY_FILE`); identity/fallback files (`<persist_dir>/state/host_id`, `/run/keys/sandbox-id`, `/proc/sys/kernel/hostname`). Container env templates set these vars (Dockerfiles, k8s podtemplate, gcp startup scripts). DB migrations `0001..0014_*.sql` are `include_str!`-embedded (`LATEST_MIGRATION_VERSION=14`).

### Notes

**nomad-addr loopback guard (fail-CLOSED, r27-S1 Guard A).** `validate_nomad_addr_loopback` (config.rs:514) — refuses boot unless host is `localhost`/`127.0.0.0/8`/`::1`. Remote DNS names and non-loopback IPs (incl. RFC1918) rejected. Prevents a tampered/remote Nomad agent from pinning jobs to the wrong node (cluster-wide DoS) or staging disk images on the wrong host.

**Other `NomadCHConfig::validate()` boot-refusals (config.rs:603):** `vm_index_floor<1`; `floor>ceil`; `ceil>155` (IP octet overflow); scheme missing; `subnet_second_octet` ∈ {127,169,224..=255}; non-canonical `user_home_dir_root` descendant; `nomad_stop_concurrency==0`.

**Fail-CLOSED feature flags.** `SANDBOX_WAKE_RESPONSE_MODE` returns Err on any value other than sync/async/empty — explicitly NOT a silent fallback (R16-S4, mirrors AEAD posture). `WakeLifecycleConfig::from_env` rejects sub-minimum retention/takeover.

**Snapshot/persistence interlocking guards (lib.rs):** (1) snapshot_enabled but persistence None ⇒ abort unless `ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION=1`; (2) snapshot_enabled + use_gcs + no KEK ⇒ abort unless `ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1`; (3) use_gcs + enabled + no bucket ⇒ hard error.

**Admin-token guards.** Both bearer files validated (mode 0o400, owner uid 0, non-empty); `assert_distinct_admin_tokens` refuses boot if Full == RO. Read once at boot — rotation requires restart.

**HA lease invariant.** `SANDBOX_HA_LEASE_TTL_SECS >= 4 × SANDBOX_HA_HEARTBEAT_SECS` and both >0 (db.rs:1133-1156), validated when pg enabled.

**Dev/test-only vars (do NOT promote to prod surface):** `SANDBOX_ALLOW_NO_AUTH`, `SANDBOX_PG_OPTIONAL=1`, `ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION`, `ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE` (the `ZEROSHIP_SANDBOX_TEST_` prefix is deliberate so an operator unit-file can't confuse them with prod), `GCS_TEST_BUCKET` (`#[cfg(test)]` only). `SANDBOX_HOST_ID` accepts raw UUID as a local-testing convenience.

**Surprises / asymmetries:**
- **Three env-parse idioms** with different failure semantics: `config.rs`/`db.rs` `parse_env*` *propagate* errors (boot fails on garbage); `sweep.rs` `read_*_env`, `SANDBOX_HA_DRAIN_GRACE_SECS`, agent `SANDBOX_AGENT_WS_PORT`/body caps **silently default on bad input**. `SANDBOX_AGENT_PORT` hard-errors.
- **Bool convention inconsistent:** `config.rs` flags parse via `FromStr` (accept only `true`/`false`); `SANDBOX_PG_RUN_MIGRATIONS`/`SANDBOX_PG_OPTIONAL`/`SANDBOX_PERSIST_AUTH`/`SANDBOX_HA_AUTO_TAKEOVER` + test overrides use exact-`"1"`. Scripts set `SANDBOX_SNAPSHOT_ENABLED=true` (config.rs bool) but `SANDBOX_PERSIST_AUTH=1` (`=="1"` style).
- **Test fixtures diverge from prod defaults:** `new_fixture` uses `vm_index_ceil=200` (would *fail* validate) / `release_delay=5`; env defaults are 20 / 2.
- **Port-derivation:** controller preview WS default `9092` is hard-coded (not derived from `SANDBOX_PORT`); agent WS `7778` is HTTP+1 by convention but is its own env var.
- **History-driven defaults** bumped after cluster stress (alloc-running 60→120, host-fence 30→120, vm_index_release 5→2).
- `SANDBOX_NOMAD_STOP_CONCURRENCY` has a live runtime gauge (`sandbox_nomad_stop_permits_in_use`).

No rows truncated — every distinct `env::var`/`parse_env`/`read_*_env` call site in `crates/sandbox/src/` + `crates/sandbox-agent/src/`. (`PATH` read in snapshot_handler.rs:576 is a `ch-remote` lookup, not config.)
## Library crates, CLI & platform (core, runtime, plugins, drivers, cli, platform, authz, bundle)

| Name | Kind | Type | Default | Secret? | Refuses boot if unset? | Controls | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `serve <file>` | CLI flag (positional) | path | required | no | exits if missing/not a file | `zeroship serve` entry JS file | crates/cli/src/main.rs:48 |
| `--port=` | CLI flag | u16 | `3000` | no | no | dev server listen port | crates/cli/src/main.rs:51 |
| `--workers=` | CLI flag | usize | `0` (auto) | no | no | dev worker-thread count | crates/cli/src/main.rs:52 |
| `--cpu-limit=` | CLI flag | u64 ms | none | no | no | per-request CPU limit (ms) | crates/cli/src/main.rs:55 |
| `--wall-timeout=` | CLI flag | u64 ms | none | no | no | per-request wall timeout (ms) | crates/cli/src/main.rs:58 |
| `--heap-limit-mb=` | CLI flag | usize MB | `512` | no | no | V8 heap limit; overrides env | crates/cli/src/main.rs:65 |
| `ZEROSHIP_HEAP_LIMIT_MB` | env | usize MB | `512` (if flag absent) | no | no | V8 heap limit fallback (CLI reads env; runtime takes it as a struct field, never reads env) | crates/cli/src/main.rs:66 |
| `DATABASE_URL` | env | URL str | none (db plugin off if unset) | yes (DSN) | no | opt-in: registers `env.db.*` in dev `serve` | crates/cli/src/main.rs:106 |
| `ZEROSHIP_STORAGE_ROOT` | env | path | `.zeroship/storage` | no | no | storage-plugin root dir (dev serve) | crates/cli/src/main.rs:117 |
| `ZEROSHIP_KV_URL` | env | URL str | none (→redb) | yes (may carry creds) | no | KV backend select: Redis if set | crates/cli/src/main.rs:130 |
| `ZEROSHIP_KV_PATH` | env | path | `.zeroship/kv.redb` | no | exits if redb open/dir-create fails | redb KV file path (when no KV_URL) | crates/cli/src/main.rs:138 |
| process env (all vars) | env (bulk) | map | — | mixed | no | `std::env::vars()` forwarded into V8 `process.env` (so `OPENAI_API_KEY`, vite's `ZEROSHIP_ENTRY`/`ZEROSHIP_VITE_*` reach JS) | crates/cli/src/main.rs:170 |
| `deploy <path>` | CLI flag (positional) | path | required | no | exits if missing | `.zship` archive to upload | crates/cli/src/main.rs:200 |
| `--app=` | CLI flag | str | required | no | panics if missing | target app for deploy/secret/var | crates/cli/src/main.rs:203; secrets.rs:48 |
| `--control=` | CLI flag | URL | `http://localhost:9090` | no | no | control-plane base URL | crates/cli/src/main.rs:204; secrets.rs:49 |
| `ZEROSHIP_CONTROL_URL` | env | URL | `http://localhost:9090` | no | no | control-plane URL fallback when `--control=` absent | crates/cli/src/main.rs:205; secrets.rs:50 |
| `--token=` | CLI flag | str (PAT) | — | yes (bearer) | no (falls through to env/creds) | deploy/secret/var bearer (1st precedence) | crates/cli/src/main.rs:349 |
| `ZS_TOKEN` | env | str (PAT) | — | yes (bearer) | no (falls through to creds file) | bearer fallback (2nd precedence) | crates/cli/src/main.rs:335 |
| `--auth-url` / `--auth-url=` | CLI flag | URL | `https://auth.zeroship.ai` | no | no | `zeroship login` OAuth IdP base URL | crates/cli/src/auth.rs:59-61 |
| `ZEROSHIP_CONFIG_HOME` | env | path | — (1st of config-dir chain) | no | no | overrides config dir for CLI token store | crates/cli/src/auth.rs:272 |
| `XDG_CONFIG_HOME` | env | path | — (2nd of chain) | no | no | config dir for CLI token store | crates/cli/src/auth.rs:274 |
| `HOME` | env | path | — (3rd; `$HOME/.config`) | no | login/token ops error "HOME is not set" if none of 3 set | base for CLI token store | crates/cli/src/auth.rs:276 |
| `ZEROSHIP_LOG_FORMAT` | env | enum `pretty\|compact\|json\|logfmt\|bunyan` | TTY→`pretty`, else `json` | no | no | tracing subscriber output format (every binary) | crates/core/src/observability.rs:36 |
| `RUST_LOG` | env | EnvFilter directive | per-binary `default_filter` | no | no | tracing log-level filter (via `EnvFilter::try_from_default_env`) | crates/core/src/observability.rs:33 |
| `ZEROSHIP_DEV` | env | presence flag | unset (= secure prod) | no | no | dev mode: disables SSRF host/IP filtering for fetch + WS + cyper resolver | crates/runtime/src/transport/ssrf.rs:89,144; client.rs:26 |
| `ZEROSHIP_STREAM_GLOBAL_CAP` | env | usize bytes | `DEFAULT_STREAM_GLOBAL_CAP` (cached via OnceLock) | no | no | process-wide stream buffer byte cap | crates/runtime/src/core/channel.rs:116 |
| `ZEROSHIP_LOG` | env | presence flag | unset (logs only in debug) | no | no | mirrors app `console.*` to operator tracing in release builds | crates/runtime/src/core/init.rs:1189 |
| `AUTH_INSECURE_DEV` | env | bool-ish (`1\|true\|yes\|on`) | unset (= internal 5xx bodies hidden) | no | no | exposes verbose internal dispatch-error bodies to clients (dev) | crates/runtime/src/core/dispatch.rs:143 |
| `ZEROSHIP_DEPLOY_ID` | env | str | `cold_start` | no | no | deploy-id stamped into migration/mask-backfill audit rows | crates/plugin-db/src/register_model/mod.rs:148; migrations.rs:390; crud/mask_backfill.rs:644 |
| `ZEROSHIP_COLUMN_KEY_<KEYID>` | env (dynamic name) | 64-hex (32 B) | required (per referenced key id) | yes (column-encryption root key) | no boot gate; first encrypted-column op fails `column_key_not_configured` if unset/malformed | per-key-id root key for `env.db` column encryption | crates/plugin-db/src/encryption/keys.rs:178-203 |
| `CARGO_PKG_VERSION` | compile-time `env!` | str | build-time | no | n/a | stamps `compiler: zeroship-passthrough@<ver>` into passthrough Manifest | crates/bundle/src/manifest.rs:232 |
| `ZEROSHIP_SESSION_SECRET` | env | hex (32 B) | required (for gated path) | yes (HMAC) | `from_env()` returns `not_configured`; backend boots, defers to first mint | **(test-helpers-gated)** SQLite session-minter active HMAC secret — NOT compiled into a normal release `plugin-db` | crates/plugin-db/src/backend/sqlite/session_minter.rs:62,106 |
| `ZEROSHIP_SESSION_SECRET_PREV` | env | hex (32 B) | optional | yes (prev HMAC) | no | **(test-helpers-gated)** SQLite session-minter rotation grace secret | session_minter.rs:67,124 |
| `ZEROSHIP_SESSION_NONCE_CAPACITY` | env | usize | `10_000` | no | no | **(test-helpers-gated)** SQLite session-minter nonce-LRU capacity | session_minter.rs:72,135 |
| `PG_TEST_URL` | env | URL | skip test if unset | yes (DSN) | no | **(test-only)** gates PG integration tests | compio-postgres/tests/integration.rs:16; plugin-db/tests/* |
| `REDIS_TEST_URL` | env | URL | skip test if unset | yes (DSN) | no | **(test-only)** gates Redis/KV integration tests | compio-redis/tests/*; plugin-kv/tests/redis_backend.rs:16 |
| `DRAGONFLY_CLUSTER_SEEDS` | env | CSV of URLs | skip test if unset | yes (DSNs) | no | **(test-only)** gates live Dragonfly-cluster tests | compio-redis/tests/cluster.rs:14; plugin-kv/tests/* |
| `KV_REQUIRE_REDIS` | env | `=="1"` | unset (skip allowed) | no | test PANICS when `=1` but `REDIS_TEST_URL` unset | **(test-only)** forces Redis KV tests in CI | plugin-kv/tests/redis_backend.rs:28 |
| `AUTH_DB_URL` | env | URL | skip test if unset | yes (DSN) | no | **(test-only)** gates DPoP/authz cross-instance PG tests (reads in `#[cfg(test)]` mod) | core/src/dpop.rs:1137,1204 (cfg(test)@1046); authz/tests/two_call_test.rs:265 |
| `AUTH_HYDRA_ADMIN` | env | URL | skip test if unset | no | no | **(test-only)** gates OIDC-verify test needing Hydra | core/tests/oidc_verify_test.rs:15 |
| `CARGO_BIN_EXE_zeroship` | compile-time `env!` | path | build-time | no | n/a | **(test-only)** locates `zeroship` binary for CLI login tests | cli/tests/login_test.rs:90,171,209 |
| `CARGO_MANIFEST_DIR` | compile-time `env!` | path | build-time | no | n/a | **(test-only)** resolves superjson fixture paths | core/tests/superjson_test.rs:12; runtime/tests/rpc_superjson.rs:58 |

### Config files

- **`zeroship` CLI credential/token file** — `<config-base>/zeroship/token.json`, where `<config-base>` = `$ZEROSHIP_CONFIG_HOME` → `$XDG_CONFIG_HOME` → `$HOME/.config` (error "HOME is not set" if none). Resolver `crates/cli/src/auth.rs:271-282`. JSON shape `Credentials { access_token, refresh_token, expires_at, auth_url, client_id }` (auth.rs:15-22). Written 0600 on Unix; written by `login`/refresh, read by `deploy`/`secret`/`var`/`whoami`, deleted by `logout`. **Only on-disk config the CLI owns — no TOML/dotfile; all CLI options are flags or env (manual `std::env::args()` parsing, no clap).**
- **No `.toml`/structured config files read by any crate in this slice.** `zeroship-platform` parses TOML via `metering/config.rs` + `core/config.rs`, but from caller-supplied `&str`/`toml::Value`, not from a config-file path or env (see Notes).
- **`DATABASE_URL` / `ZEROSHIP_KV_URL`** are commonly sourced from a project `.env` by the vite-plugin, which forwards them into the `zeroship serve` child env; the CLI itself reads them only from the process environment (no `.env` parsing in-crate).

### Notes

- **CLI uses no arg-parsing library.** `crates/cli/src/main.rs:28` does `std::env::args().collect()` + positional match; hand-parsed flags (`flag_str`/`flag_u16`/`flag_value`). Zero clap attributes in this slice.
- **`crates/platform` reads NO env vars and NO CLI flags.** Library (`zeroship-platform`, no main). `metering/config.rs` + `core/config.rs` deserialize TOML from in-memory `&str`/`toml::Value` passed by callers; `metering/store/sqlite.rs:28` parses a `database_url: &str` argument. If platform config is to be unified, its inputs arrive as constructor args/TOML blobs, not process config.
- **`crates/runtime-macros` and `crates/authz` have zero production config inputs** (authz: only `AUTH_DB_URL` in a test file).
- **`crates/plugin-storage`, `crates/plugin-kv`, `crates/compio-postgres`, `crates/compio-redis` read NO env in `src/`** — all env usage is in `tests/`. Backend URLs/paths are passed in as constructor args by the CLI/host (`KvPlugin::with_backend`, `DbPlugin::new(url)`, `StoragePlugin::new(root)`). `bundle`'s only config input is compile-time `CARGO_PKG_VERSION`.
- **Critical test-vs-prod distinction — SQLite session secrets.** `ZEROSHIP_SESSION_SECRET`, `_PREV`, `_NONCE_CAPACITY` (and the whole `SqliteSessionMinterConfig::from_env()` + `new_with_secrets` path) are **all `#[cfg(any(test, feature = "test-helpers"))]`-gated** (session_minter.rs:61-152; backend/sqlite/mod.rs:404-417). A normal release build of `plugin-db` never reads them. Treat as **test-only today**, but flag for the design: a production SQLite-auth path would need a non-gated config source.
- **`ZEROSHIP_DEPLOY_ID` is genuinely production** (no cfg gating; live async migration/backfill). Default string `"cold_start"`.
- **`ZEROSHIP_COLUMN_KEY_<KEYID>` is a dynamically-named secret** (suffix = uppercased key id from the encryption policy). One env var per encryption key. No boot gate — failure surfaces at first encrypted-column op as `DbError::Configuration { code: "column_key_not_configured" }`.
- **Doc/code drift:** `observability.rs` doc says the filter env is `RUST_LOG` (correct), but `init.rs:1189` uses a *separate* `ZEROSHIP_LOG` presence flag for console mirroring — two different vars with overlapping-sounding names.
- **Presence-only boolean envs** (`ZEROSHIP_DEV`, `ZEROSHIP_LOG`) checked with `.is_ok()` — any value (even empty) enables. `AUTH_INSECURE_DEV` instead requires an explicit truthy token (`1/true/yes/on`).
- **No crate in this slice "refuses boot if unset"** for any production var — every production input has a default or defers failure to first use. Only hard-stops are CLI arg-validation `exit(1)`s (missing serve file, missing `--app=`, redb open failure) and the test-only `KV_REQUIRE_REDIS=1` panic.

No rows truncated; complete inventory for the 12 in-scope crates.
## TypeScript/JS — apps & SDKs

Covers `apps/` (only `zeroship-builder`) and `sdks/`. Three groups: (1) the **builder app's server runtime** reads env via a `readEnv()` helper with a `process.env` → `globalThis.env` → fallback chain (set by `zeroship secret set` in prod, or the host shell in dev); (2) the **vite-plugin** reads env at dev/build time; (3) the **client browser bundle** reads only `import.meta.env.DEV`. The `@zeroship/control` SDK and `create-zeroship-app` take **no env** — control is configured by a constructor options object; the scaffolder reads only `process.argv`.

| Name | Kind | Type | Default | Secret? | Build/runtime? | Controls | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `ZEROSHIP_CONTROL_URL` | env (process.env) | URL | falls through to `CONTROL_URL` | no | runtime (builder server) | Control-plane origin (primary key) | apps/zeroship-builder/src/server/internal/env.ts:31; control-client.ts:231 |
| `CONTROL_URL` | env | URL | `http://localhost:9090` | no | runtime (builder server) | Control-plane origin (fallback key) | internal/env.ts:33; control-client.ts:233 |
| `SANDBOX_URL` | env | URL | `http://localhost:9091` | no | runtime (builder server) | Sandbox controller base URL for fs/exec tools | internal/env.ts:36 |
| `SANDBOX_TOKEN` | env | string | `test` | **yes** | runtime (builder server) | Bearer to sandbox controller; must match controller's `SANDBOX_TOKEN` (≥32 B) | internal/env.ts:38 |
| `OPENAI_API_KEY` | env | string | `""` | **yes** | runtime (builder server) | OpenAI key for builder agents | internal/env.ts:40; also chat.ts:175, wizard.ts:194, pm-worker.ts:107, sre-worker.ts:110 |
| `HYDRA_AUTHORIZE_URL` | env | URL | `http://localhost:4444/oauth2/auth` | no | runtime (builder server) | OIDC authorize endpoint (OAuth delegation) | oauth.ts:42 |
| `HYDRA_TOKEN_URL` | env | URL | `http://localhost:4444/oauth2/token` | no | runtime (builder server) | OIDC token endpoint | oauth.ts:134 |
| `HYDRA_REVOKE_URL` | env | URL | `http://localhost:4444/oauth2/revoke` | no | runtime (builder server) | OIDC revoke endpoint | oauth.ts:92 |
| `HYDRA_JWKS_URL` | env | URL | `http://localhost:4444/.well-known/jwks.json` | no | runtime (builder server) | JWKS for access-token verification | oauth.ts:113 |
| `HYDRA_ISSUER` | env | string | `""` (skips iss check if empty) | no | runtime (builder server) | Expected JWT `iss` | oauth.ts:124 |
| `BUILDER_ACCESS_TOKEN_AUDIENCE` | env | string | `""` (skips aud check if empty) | no | runtime (builder server) | Expected JWT `aud` | oauth.ts:126 |
| `BUILDER_CLIENT_ID` | env | string | `zeroship-builder` | no | runtime (builder server) | OAuth client_id for builder RP | oauth.ts:203 |
| `BUILDER_CLIENT_SECRET` | env | string | `""` | **yes** | runtime (builder server) | OAuth client secret | oauth.ts:207 |
| `BUILDER_REDIRECT_URI` | env | URL | `http://localhost:3001/auth/callback` | no | runtime (builder server) | OAuth redirect URI | oauth.ts:211 |
| `BUILDER_TOKEN_ENCRYPTION_KEY` | env | string | none (throws if unset) | **yes** | runtime (builder server) | Key for encrypted OAuth-token storage; fallback for `BUILDER_COOKIE_SECRET` | oauth-store.ts:143; session.ts:61 |
| `BUILDER_COOKIE_SECRET` | env | string | falls back to `BUILDER_TOKEN_ENCRYPTION_KEY` | **yes** | runtime (builder server) | HMAC secret for signed `__zs_builder_oauth_user` cookie | session.ts:60 |
| `BUILDER_INSTANCE_ID` | env | string | `local` | no | runtime (builder server) | Namespacing for OAuth token store entries | oauth-store.ts:158 |
| `ZEROSHIP_BUILDER_DISABLE_DEV_USER` | env | `"1"` flag | unset (dev-user enabled) | no | runtime (builder server) | When `"1"` (or `NODE_ENV=production`) disables local dev auto-user | internal/sandbox-backend.ts:130 |
| `NODE_ENV` | env | string | unset | no | runtime (builder server) | `=== "production"` disables dev auto-user | internal/sandbox-backend.ts:130 |
| `import.meta.env.DEV` | import.meta.env | boolean | Vite-injected | no | **build-time** (client bundle) | Gates dev-only UI + `isDevAutoAuth()` | client/api.ts:23; App.tsx:67; DevEventsBadge.tsx:40 |
| `ZEROSHIP_BUILDER_API_PORT` | env | number | `3002` | no | **build-time** (vite.config) | Port passed to `zeroship({ devServerPort })` | apps/zeroship-builder/vite.config.ts:16 |
| `CONTROL_URL` | env | URL | `http://localhost:9090` | no | **build-time** (vite.config) | Vite dev-server `/auth` proxy target | vite.config.ts:17 |
| `DATABASE_URL` | env | URL | `sqlite:.zeroship/dev.sqlite` | maybe (PG creds) | dev-time (vite-plugin) | DB URL injected into spawned dev runtime; shell env → `.env` → SQLite default | sdks/vite-plugin/src/dev-server.ts:91,94,415,430; dev-db.ts:13 |
| `ZEROSHIP_BIN` | env | path | `node_modules/.bin/zeroship` or PATH | no | dev-time (vite-plugin) | Override for the `zeroship` binary spawned | dev-server.ts:353 |
| `ZEROSHIP_DEV` | env (set by vite-plugin) | `"1"` flag | injected `=1` | no | dev-time (vite-plugin → runtime) | Marks spawned runtime as dev mode | constants.ts:8; set dev-server.ts:431 |
| `ZEROSHIP_VITE_ORIGIN` | env (set by plugin; read by dev-bootstrap) | URL | injected `http://localhost:<vitePort>` | no | dev-time | Origin dev-bootstrap uses for module fetch/HMR/RPC | constants.ts:9; set dev-server.ts:432; read dev-bootstrap/index.ts:134, transport.ts:18 |
| `ZEROSHIP_ENTRY` | env (set by plugin; read by dev-bootstrap) | path | injected if `serverEntry` resolved | no | dev-time | Server entry module for dev runtime | constants.ts:10; set dev-server.ts:433; read dev-bootstrap/index.ts:35 |
| `process.env.NODE_ENV` (define) | config key (vite define) | literal `"production"` | n/a | **build-time** (SSR build) | Hard-set in worker SSR build; `process.env` preserved | sdks/vite-plugin/src/build.ts:89-90 |
| `NODE_ENV` | env | string | unset | no | runtime (db SDK) | `!== "production"` gates unindexed-filter dev warning | sdks/db/src/collection/index-warnings.ts:63 |
| `LAZY` | env | `"1"` flag | unset | no | test-only (vite-plugin) | Toggles lazy-procedure test path | sdks/vite-plugin/test/lazy-procedures.test.ts:153 |

### Config files

- **`apps/zeroship-builder/vite.config.ts`** — registers `@zeroship/vite-plugin` (`zeroship({ devServerPort })`), react, tailwind. Reads `ZEROSHIP_BUILDER_API_PORT`, `CONTROL_URL`. Build-time.
- **`apps/zeroship-builder/.env`** (gitignored) — only `OPENAI_API_KEY` set locally.
- **`apps/zeroship-builder/.env.example`** — documents `OPENAI_API_KEY`, `SANDBOX_URL`, `SANDBOX_TOKEN`, `HYDRA_{AUTHORIZE,TOKEN,REVOKE}_URL`, `BUILDER_CLIENT_ID`/`_SECRET`/`_REDIRECT_URI`/`_INSTANCE_ID`/`_TOKEN_ENCRYPTION_KEY`. **Slightly behind code** — missing `HYDRA_JWKS_URL`/`HYDRA_ISSUER`/`BUILDER_ACCESS_TOKEN_AUDIENCE`/`BUILDER_COOKIE_SECRET` which the code also reads.
- **`apps/zeroship-builder/playwright.config.ts`** — reads `PLAYWRIGHT_NO_WEBSERVER`, `CI`; webServer `npm run dev -- --port 5173`. **`playwright.m0.config.ts`** — `baseURL` from `BUILDER_URL` (default `http://localhost:3001`).
- **`package.json` scripts** — `dev: vite`, `build: tsc -b && vite build`, `deploy: vite build && zeroship deploy`, `test:e2e*: playwright test`.
- **`sdks/create-zeroship-app/template/vite.config.ts`** — minimal `zeroship()` + react. `bin/create.js` reads only `process.argv[2]`.
- **`sdks/vite-plugin/src/constants.ts`** — child-process env var names (`ZEROSHIP_DEV`, `ZEROSHIP_VITE_ORIGIN`, `ZEROSHIP_ENTRY`), `DEFAULT_DEV_PORT=3001`, `DEFAULT_RPC_ENDPOINT="/_rpc"`.
- **`@zeroship/vite-plugin` options** (`ZeroshipOptions`, src/index.ts:28) — programmatic, not env: `rpcEndpoint` (`/_rpc`), `serverEntry` (auto), `devServerPort` (3001), `mode` (full/static), `rpc.strict` (auto/always/never).
- **`@zeroship/control` options** (`ControlClientOptions`, src/index.ts:4) — programmatic, **no env**: `baseUrl` (required, throws if missing), `fetch`, `auth`, `cookie`, `headers`, `onSetCookie`. The consumer supplies control URL + token.
- **tsconfig.json / tsup.config.ts** (all SDKs) — pure build config, no env reads.

### Notes

- **Two `readEnv()` helpers, identical chain** (internal/env.ts:17; session.ts:67): `globalThis.process.env[key]` → `globalThis.env[key]` → fallback. In prod the runtime populates `process.env` from per-app vars + exposed secrets (`crates/runtime` init.rs `setup_globals`); in dev the vite-plugin forwards host shell env / `.env`.
- **Build-time vs runtime split:** the **client** bundle only sees `import.meta.env.DEV` (static-replaced by Vite). All real config is **server-side runtime** env inside `"use server"` modules — never shipped to the browser.
- **`control-client.ts` has its own `ControlClient`** distinct from `@zeroship/control` SDK — reads the same `ZEROSHIP_CONTROL_URL`→`CONTROL_URL` chain (`controlBaseUrl()` line 230).
- **Stale config references (not live code):** `client/builder/README.md:68-70` documents `VITE_PROXY_CONTROL`, `VITE_PROXY_GATEWAY`, `VITE_AGENT_URL` — none read anywhere in current source. Outdated docs.
- **Runtime-side env documented in SDK source but NOT read by JS** (consumed by the Rust runtime; surfaced so it isn't double-counted against the Rust inventory): `ZEROSHIP_KV_PATH`/`ZEROSHIP_KV_URL` (sdks/kv/src/index.ts:17-18), `ZEROSHIP_COLUMN_KEY_<KEYID>` (sdks/db/src/types.ts:696), `ZEROSHIP_MODULE_JS` (runtime-synthesized virtual module).
- **SDK env reads are essentially nil** — the only non-test JS `process.env` read in SDK source is `NODE_ENV` in `@zeroship/db`. Everything else is programmatic options or runtime-injected globals.
- **No `VITE_*` variables are actually consumed** in live source (only the stale README). No `.env.local`/`.env.production` files in scope — only the builder's `.env` + `.env.example`.
- **Test/e2e env vars** (Playwright + `*.test.ts`): `CONTROL_URL`, `CONTROL_KEY` (default `dev-master-key`, secret), `SANDBOX_URL`, `GATEWAY_URL`, `BUILDER_URL`, `OPENAI_API_KEY`, M0-gate knobs (`M0_RESULTS_PATH`, `M0_PROMPT_TIMEOUT_MS`, `M0_REACHABLE_TIMEOUT_MS`, `M0_MAX_SURVEY_RESUMES`, `M0_ONLY`, `M0_LIMIT`, `M0_RESUME`, `M0_EXPECT_MIN`), OAuth fixtures. Excluded from the main table (configure tests, not the product surface) except the `LAZY` row.

Nothing truncated; only deliberate omission is per-test e2e env (summarized above).
## Ops, deploy, infra & the Go ch-driver

### Config files

| Path | Format | Purpose | Notable keys/fields |
| --- | --- | --- | --- |
| `ops/auth-clients.example.toml` | TOML | Declarative OIDC client registry; auth reconciles against Hydra admin at every boot (upsert, never deletes) | `[[client]]` array. `console.zeroship.ai` (secret `dev-secret-rotate-me`, redirect/post-logout/backchannel URIs, scope, `access_token_strategy=jwt`, `id_token_signed_response_alg=EdDSA`, audience, first_party); `zeroship-cli` (device_code+refresh, `token_endpoint_auth_method=none`, scopes `apps:deploy apps:read`); `gateway` (secret `dev-secret-rotate-me-too`, empty redirect_uris grown per-deploy, stable backchannel_logout_uri) |
| `ops/hydra.yaml` | YAML | Ory Hydra OIDC kernel config (mounted at `/etc/config/hydra/hydra.yaml`) | `dsn` (hardcoded `postgres:zeroship@postgres:5432`); `serve.public.port=4444`, `serve.admin.port=4445`, `serve.cookies` (`same_site_mode=Lax`, `domain=auth.zeroship.ai`); `urls.self.issuer/public`, login/consent/logout/error URLs; `strategies.access_token=opaque`; `oauth2.pkce.enforced=true`; `oauth2.grant.refresh_token.rotation_grace_period=30s`/`rotation_grace_reuse_count=3`; `device_authorization.token_polling_interval=5s`; `ttl.{access_token=1h,refresh_token=720h,id_token=1h,auth_code=60s,login_consent_request=1h}`; `oidc.dynamic_client_registration.enabled=false`; `log.level=info`/`format=json` |
| `Dockerfile` (root) | Dockerfile | Builds all platform binaries (control, gate, worker, sandbox, `zeroship` CLI); `rust:latest` → `ubuntu:24.04` | No `ENV`. Installs `ca-certificates curl docker.io`. Copies 5 release binaries to `/usr/local/bin/` |
| `.dockerignore` | ignore | Build-context exclusions | `target/`, `.git/`, runtime benches, `crates/platform/`, `docs/`, `tests/`, `*.md` |
| `docker-compose.yml` | Compose YAML | Day-to-day local multi-node stack (1 gw + 3 workers) | Services: `postgres` (16, 5440→5432), `control`, `gateway`, `worker` (replicas:3), `hydra` (oryd/hydra:v25.4.0), `hydra-migrate`, `sandbox`, `builder` (node:22 Vite dev). Volumes `bundles`, `builder-pnpm-store`; net `zeroship-sandbox-net`. **Binaries configured via `command:` flags, not env** (see Notes) |
| `docker-compose.cluster.yml` | Compose YAML | Separate 3-node Dragonfly (Redis) cluster for compio-redis tests; does NOT boot platform | `dragonfly-0/1/2` (`dragonflydb/dragonfly:latest`), `network_mode: host`, 7000/7001/7002. CLI flags only |
| `docker/agent-runtime/Dockerfile` | Dockerfile | Agent runtime image — PID 1 inside libkrun/CH microVM; Node22+pnpm+git + `sandbox-agent` on :7777 | `ENV DEBIAN_FRONTEND`, `NODE_VERSION=22.11.0`, `PATH`, `LANG`, `LC_ALL`. `EXPOSE 7777`. `ENTRYPOINT sandbox-agent` |
| `docker/sandbox-base/Dockerfile` | Dockerfile | `zeroship/sandbox-base` — one container per editor session (Docker backend); node:22-alpine | No runtime `ENV`. Git `--system` config. `EXPOSE 5173`. `ENTRYPOINT sandbox-entrypoint.sh`, `CMD sleep infinity` |
| `docker/sandbox-base/{entrypoint,agent-entrypoint,build}.sh` | sh | Seed empty `/workspace` from template, git-init, exec | `TAG=${1:-latest}`, `IMAGE=zeroship/sandbox-base` (build.sh); no runtime env |
| `apps/zeroship-builder/.env.example` | dotenv | Template for Builder web-app secrets | `OPENAI_API_KEY`, `SANDBOX_URL=http://localhost:9091`, `SANDBOX_TOKEN`, `HYDRA_{AUTHORIZE,TOKEN,REVOKE}_URL`, `BUILDER_CLIENT_ID=zeroship-builder`, `BUILDER_CLIENT_SECRET`, `BUILDER_REDIRECT_URI=http://localhost:3001/auth/callback`, `BUILDER_INSTANCE_ID=local`, `BUILDER_TOKEN_ENCRYPTION_KEY` |
| `nomad-driver-ch/ch/driver.go` (`configSpec`) | Go/hclspec | Driver-level Nomad `plugin "nomad-driver-ch" { config {} }` schema | `cloud_hypervisor_bin` (`/usr/local/bin/cloud-hypervisor`), `ch_remote_bin` (`/usr/local/bin/ch-remote`), `virtiofsd_bin` (`/usr/local/bin/virtiofsd`), `vm_index_lockdir` (`/var/lib/zsbx/vm-index`), `run_dir` (`/var/lib/zsbx/run`), `content_addressed_rootfs_roots` (list(string), opt) |
| `nomad-driver-ch/ch/task_config.go` (`taskConfigSpec`) | Go/hclspec | Per-task `config {}` decoded from controller jobspec (msgpack `codec:`) | `vm_index`, `kernel`, `cmdline`, `cpus`, `memory_mb`, `disks[]{path,readonly,serial}`, `fs[]{tag,socket,source_path}`, `net[]{tap,mac,ip,mask}`, `restore_from`, `sandbox_id`, `user_id`, `workspace_img`, `user_home_img`, `rootfs_source`, `pubkey_hex`, `subnet_base_octet` (default 99), `stage_disk_images` |
| `nomad-driver-ch/{flake.nix,go.mod,Makefile,…}` | various | Go build/dev tooling | no runtime config keys |

No standalone `.hcl` jobspec files — the Nomad job + task `Config` is generated programmatically by `crates/sandbox/src/backend/nomad_ch.rs`. The only checked-in HCL is the inline Nomad client/plugin stanza in `gcp-worker-startup.sh` + an example in `docs/runbooks/sandbox-nomad-ch.md`. `policies/{creator,platform}/*.cedar` are pure Cedar authz rules — **no operational config**.

The TaskConfig fields `sandbox_id`/`workspace_img`/`user_home_img`/`pubkey_hex`/`subnet_base_octet` each map 1:1 to a `ZSBX_*` env var of the legacy bash wrapper (`crates/sandbox/scripts/nomad-vm-wrapper.sh`), but the **Go driver decodes them from the HCL/msgpack jobspec, not from env**.

### Env vars set/consumed in deploy & scripts

| Name | Where (file:line) | Value/Default | For which binary | Notes |
| --- | --- | --- | --- | --- |
| `POSTGRES_PASSWORD` | docker-compose.yml:13 | `zeroship` | postgres | |
| `POSTGRES_DB` | docker-compose.yml:14 | `zeroship` | postgres | |
| `GATEWAY_OIDC_SECRET` | docker-compose.yml:78 | `${GATEWAY_OIDC_SECRET:-dev-secret-rotate-me-too}` | zeroship-gate | matches `gateway` client in ops TOML |
| `STASH_SIGNING_KEY` | docker-compose.yml:82 | `${STASH_SIGNING_KEY:-dev-stash-key-please-rotate}` | zeroship-gate | HMAC key for `__Host-zs_oidc_stash` |
| `DSN` | docker-compose.yml:132,156 | `postgres://postgres:zeroship@postgres:5432/zeroship?sslmode=disable` | hydra, hydra-migrate | overrides hydra.yaml `dsn` |
| `SECRETS_SYSTEM` | docker-compose.yml:133 | `${SECRETS_SYSTEM:-dev-secret-please-change-this-please}` | hydra | |
| `SECRETS_COOKIE` | docker-compose.yml:134 | `${SECRETS_COOKIE:-dev-secret-please-change-this-please}` | hydra | |
| `SANDBOX_PORT` | compose:171; sandbox_up.sh:12,162; gcp-worker-startup.sh:570 | `9091` | zeroship-sandbox | |
| `SANDBOX_TOKEN` | compose:172,221; sandbox_up.sh:14,163; m0_gate.sh:20; .env.example | `sandbox-key` (compose) / harness 32+B token / `sandbox-token` (gcp) | sandbox + builder | controller refuses `<32` B |
| `SANDBOX_IMAGE`/`_WORKSPACE_ROOT`/`_NETWORK`/`_AUTO_PULL`/`_IDLE_TIMEOUT_SECS`/`_MAX_LIFETIME_SECS` | compose:173-178; sandbox_up.sh:15-17,165-170 | defaults match Rust | zeroship-sandbox | docker-backend knobs |
| `SANDBOX_BACKEND` | sandbox_up.sh:164; gcp-worker-startup.sh:500 | `docker` (harness) / `nomad-ch` (prod) | zeroship-sandbox | |
| `SANDBOX_DATABASE_URL` | sandbox_up.sh:20,171; gcp-worker-startup.sh:423 | harness localhost:5440 / prod env-file | zeroship-sandbox | |
| `SANDBOX_PG_RUN_MIGRATIONS` | sandbox_up.sh:172; gcp-worker-startup.sh:556 | `1` (harness; prod only on migrator worker-1) | zeroship-sandbox | |
| `SANDBOX_PG_BOOT_TIMEOUT_SECS`/`SANDBOX_PERSIST_DIR`/`SANDBOX_HOST_ID` | sandbox_up.sh:173-175 | `60` / `$STATE_DIR/persist` / fixed UUIDv7 | zeroship-sandbox | |
| `CONTROL_URL` | compose:211; m0_gate.sh:242,282; e2e_oauth:34 | `http://control:9090` / `http://localhost:9090` | builder, e2e | |
| `CONTROL_KEY` | compose:212; m0_gate.sh:243,283 | `platform-key` / `$MASTER_KEY` | builder, control/gate (also flag) | |
| `HYDRA_AUTHORIZE_URL`/`_TOKEN_URL`/`_REVOKE_URL` | compose:213-215; .env.example | localhost:4444 / hydra:4444 | builder | |
| `BUILDER_CLIENT_ID`/`_SECRET`/`_REDIRECT_URI`/`_INSTANCE_ID`/`_TOKEN_ENCRYPTION_KEY` | compose:216-219,207; .env.example; local-dev.md:104 | `zeroship-builder` / generated / `http://localhost:3001/auth/callback` / `docker-compose` / dev key | builder | `_SECRET` from `/data/builder-client-secret` (written by control `--bootstrap-builder-client`) |
| `SANDBOX_URL` | compose:220; m0_gate.sh:19,244,284; .env.example | `http://sandbox:9091` / localhost | builder, e2e | |
| `OPENAI_API_KEY` | compose:222; m0_gate.sh:127-128,246,287; .env.example | `${OPENAI_API_KEY:-}` (required for m0) | builder | m0_gate sources from builder `.env`; fails if unset |
| `CONTROL_PORT`/`WORKER_PORT`/`GATEWAY_PORT`/`BUILDER_PORT`/`BUILDER_API_PORT` | m0_gate.sh:12-16; e2e_platform.sh:30-32 | `9090`/`8080`/`8000`/`3001`/`3002` | control/worker/gate/builder | passed as `--port` flags |
| `DATABASE_URL` | m0_gate.sh:17,194; e2e_platform.sh:33; local-dev.md:38,53 | m0 PG localhost:5440 / vite-dev `sqlite:.zeroship/dev.sqlite` | control / dev runtime | also `DB_URL` in e2e → `--db` |
| `MASTER_KEY` | m0_gate.sh:23,195; e2e_platform.sh:36; e2e_docker.sh:14 | `dev-master-key` / `test-mk` / `master-key` | control (`--master-key`), CLI deploy (`--key`) | |
| `ZEROSHIP_DEV_INSECURE` | m0_gate.sh:193 | `1` | zeroship-control | dev-insecure mode |
| `BOOTSTRAP_BUILDER_OAUTH_CLIENT` | local-dev.md:100 | `1` | zeroship-control | alt to `--bootstrap-builder-client` |
| `BUILDER_CLIENT_SECRET_FILE` | local-dev.md:104 | `data/builder-client-secret` | zeroship-control | secret output path |
| `CONTROL_URL`/`HYDRA_PUBLIC_URL`/`HYDRA_ADMIN_URL`/`AUTH_URL`/`AUTH_DB_URL` | e2e_oauth_delegation.sh:34-38 | localhost:9090/:4444/:4445/:8080/pg:5441 | OAuth e2e (assumes stack running) | |
| `NUM_WORKERS` | e2e_docker.sh:11 | `3` | compose `--scale worker` | |
| `DRAGONFLY_CLUSTER_SEEDS` | docker-compose.cluster.yml:7; docker-compose.md:38 | `redis://127.0.0.1:7000,7001,7002` | compio-redis cluster test | |
| **Go driver env** ↓ | | | | |
| `ZSBX_CH_BIN` | nomad-driver-ch/ch/ch_client.go:85,98,145 | env→`config.cloud_hypervisor_bin`→PATH | cloud-hypervisor discovery | **only** runtime env the Go driver reads (`os.Getenv`); stat-checked |
| `ZSBX_CH_REMOTE_BIN` | ch_client.go:86,103 | env→`config.ch_remote_bin`→PATH | ch-remote discovery | same precedence |
| `ZSBX_ARTIFACT_DIR` | start_task.go:62,632 | from Nomad task `cfg.Env`, NOT process env | driver — locate rootfs/vmlinuz | controller emits per-job (nomad_ch.rs:2339,2495) |
| `main.gitSHA` | cmd/.../main.go:35 | `-ldflags -X` at build | driver `--version` | link-time, not runtime |
| **Prod worker systemd unit (`gcp-worker-startup.sh`)** ↓ | | | | |
| `SANDBOX_BACKEND`/`SANDBOX_NOMAD_ADDR`/`SANDBOX_NOMAD_DATACENTER` | gcp-worker-startup.sh:500-502 | `nomad-ch` / `http://127.0.0.1:4646` / `$DATACENTER` | zeroship-sandbox | |
| `SANDBOX_NOMAD_CH_RUNTIME_DIR`/`_HOST_STATE_DIR`/`_USER_HOME_ROOT` | gcp-worker-startup.sh:503-505 | `/var/lib/zeroship/ch` / `/var/zeroship/ch` / `…/users` | zeroship-sandbox | |
| `SANDBOX_NOMAD_CH_VM_INDEX_FLOOR`/`_CEIL`/`_SUBNET_BASE_OCTET`/`_HOST_FENCE_TIMEOUT_SECS` | gcp-worker-startup.sh:506-519 | `1`/`$VM_INDEX_CEIL`/`99`/`30` (Rust default 120) | zeroship-sandbox | |
| `SANDBOX_WAKE_RESPONSE_MODE` | gcp-worker-startup.sh:530 | `async` | zeroship-sandbox | |
| `SANDBOX_SNAPSHOT_ENABLED`/`_L1_ROOT`/`_USE_GCS`/`_GCS_BUCKET` | gcp-worker-startup.sh:533-536 | `true` / `/var/zeroship/ch/snapshots` / `true` / `$SNAPSHOT_BUCKET` | zeroship-sandbox | boot-asserts persist triplet |
| `SANDBOX_PERSIST_AUTH`/`SANDBOX_AEAD_KEY_PATH`/`SANDBOX_SNAPSHOT_ROOT_KEK_PATH`/`SANDBOX_ADMIN_TOKEN_PATH` | gcp-worker-startup.sh:545-553 | `1` / `$AEAD_KEY_PATH` / `$ROOT_KEK_PATH` / `$ART/sandbox-admin-token` | zeroship-sandbox | fail-closed boot asserts |
| `SANDBOX_DRIVER_STAGES_DISK_IMAGES` | gcp-worker-startup.sh:567; sandbox-nomad-ch.md:91 | `true` | zeroship-sandbox | emits `stage_disk_images=true` in jobspec |
| `RUST_LOG` | gcp-worker-startup.sh:571 | `info,zeroship_sandbox=info` | zeroship-sandbox | |
| `SANDBOX_TOKEN`/`SANDBOX_ADMIN_TOKEN`/`SANDBOX_DATABASE_URL` | gcp-worker-startup.sh:80,81,416-425 | from GCE metadata → `*.env` (chmod 0400) via `EnvironmentFile=` | zeroship-sandbox | secrets via EnvironmentFile, not inline |
| `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP` | sandbox-nomad-ch.md:118 | `true` (single-replica only) | zeroship-sandbox | documented operator knob |
| GCP provisioning vars (`PROJECT`/`REGION`/`ZONE`/`PREFIX`/`SERVER_COUNT`/`WORKER_COUNT`/`ARTIFACT_BUCKET`/`SNAPSHOT_BUCKET`/`VM_INDEX_CEIL`) | provision-gcp-cluster.sh; sandbox-nomad-ch.md:36-43 | script-internal | provisioner (not a binary) | sets cluster metadata |

### Notes

- **Compose/CLI flags vs env — the dominant supply surface.** The three core binaries (`zeroship-control`/`-worker`/`-gate`) are configured in compose, `tests/*.sh`, and runbooks **almost entirely via CLI flags, not env**: `--port`, `--db`, `--bundles`/`--blob-store`, `--control-key`, `--master-key`, `--auth-secret`, `--control`, `--workers`, `--worker-key`, `--bind`, `--poll-interval`, `--max-isolates`, `--hydra-admin-url`/`--hydra-public`, `--auth-public`, `--dev-insecure`/`--insecure-dev`/`--bootstrap-builder-client`/`--builder-client-secret-file`. Env vars are mostly confined to the sandbox controller, builder app, hydra, and the Go driver's two binary-path overrides. (`local-dev.md:97` mentions a `--jwt-secret` flag for control, but that flag does not exist — `--jwt-secret`/`JWT_SECRET` appear zero times in `crates/`; only gateway has `--auth-secret`/`AUTH_SECRET`. The runbook line is stale and should be deleted.)
- **Worker safety gate** (compose:95-112): worker defaults to `127.0.0.1`; binding `0.0.0.0` requires BOTH `--bind 0.0.0.0` AND `--worker-key`.
- **Go driver is env-lean by design.** Only `os.Getenv` in the whole `nomad-driver-ch/` tree (excluding tests) is `lookupBin` for `ZSBX_CH_BIN`/`ZSBX_CH_REMOTE_BIN`. Everything else arrives via Nomad HCL→msgpack: driver `Config` (6 fields) + per-task `TaskConfig` (~17 fields). `ZSBX_ARTIFACT_DIR` is read from the **task's** `cfg.Env`, not driver process env. The many `ZSBX_*` strings in comments/tests document the legacy bash wrapper the Go driver replaced.
- **`SANDBOX_*`/`SANDBOX_SNAPSHOT_*` consumers are Rust** (`crates/sandbox`), but **set** by `gcp-worker-startup.sh` + documented in `sandbox-nomad-ch.md`.
- **Secrets posture.** Every secret in checked-in files is a rotate-me dev placeholder. Production secrets flow via GCE metadata → `EnvironmentFile=` (chmod 0400) for the sandbox controller, and via file paths (`SANDBOX_AEAD_KEY_PATH`, `SANDBOX_SNAPSHOT_ROOT_KEK_PATH`, `SANDBOX_ADMIN_TOKEN_PATH`, `--builder-client-secret-file`).
- **Driver version pin** is a GCS artifact name, not env: `gcp-worker-startup.sh:171` pulls `nomad-driver-ch.v25`. Nomad requires an explicit `plugin "nomad-driver-ch" { config {} }` stanza (Nomad 2.0.2). Worker `data_dir` hard-pinned to `/opt/nomad/data`.
- **Scope boundaries:** `policies/*.cedar` = authz rules, no config. `apps/zeroship-builder/.env*` and `crates/sandbox/scripts/*.sh` are just outside `scripts/`+`tests/`+`ops/`+`docker/` but are the deploy tooling the in-scope runbook designates source-of-truth, so included. **Not exhaustively catalogued (out of scope):** `crates/sandbox/scripts/{provision-gcp-cluster,gcp-server-startup,bake-rootfs,init,lint,teardown}.sh` + `snapshot_stress.py` (captured only the env they set ON the binaries). `.envrc` files are direnv shims; `refs/` `.env` files are vendored upstream fixtures.
