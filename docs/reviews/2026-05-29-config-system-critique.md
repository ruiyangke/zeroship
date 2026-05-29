# Server-config system — consolidated critique (2026-05-29)

Four independent critics reviewed the unified server-configuration system on
`feat/server-config-unification` (the 14-commit clap+TOML unification **plus** the
uncommitted well-known-path auto-discovery work):

- **architecture-critic** (`a266e4e9`) — boundaries, god-module, duplication, extensibility
- **code-critic / security** (`a8b6d213`) — compiled clap 4.6.1 + the master-key decoder to verify behaviors
- **design-critic** (`a95f57fd`) — coherence, operator ergonomics, docs accuracy, AI-friendliness
- **codex** (independent model) — full end-to-end pass
- plus the auto-discovery workflow's own 4-lens internal review

Findings are deduped and ranked by severity, with **[sources]** noting cross-critic
agreement (more sources = higher confidence). Items the maintainer (me) verified
directly against the code are marked **✔ verified**. Items about the *new auto-discovery
delta* (vs the pre-existing unification) are marked **(AD)**.

---

## Overall verdict

The precedence *mechanism* (clap `Option<T>` → `.or(file).or(default)`) and the D1–D9 flag
unification for the three legacy binaries are genuinely good and well-tested. But the
system is **unified in name more than in substance**: resolution, validation, boolean
precedence, `--check-config`, security policy, and docs are still per-binary hand-work,
and the **fourth binary (auth) runs a parallel, divergent idiom** (every critic flagged
this). The most serious issues are security-relevant and several are pre-existing, not
introduced by auto-discovery.

No **Critical-with-active-exploit** in a default prod boot, but two High items combine
into a Critical-class exposure under a single stray env var.

---

## HIGH / CRITICAL (security & correctness)

### S1. `--dev-insecure` is un-overridable from the CLI **and** control/gateway/auth bind `0.0.0.0` → a single leftover `ZEROSHIP_DEV_INSECURE=1` exposes an unauthenticated control plane. ✔ verified
[codex C1+C2, security M3+M6+L4]
- `insecure_dev = --dev-insecure flag || env_is_exact("ZEROSHIP_DEV_INSECURE","1")` with no `--no-dev-insecure`, so env wins and CLI cannot turn it off (`control/main.rs:87`, `gateway:129`, `worker:47`). Violates the advertised "CLI > env" precedence.
- Insecure mode disables admin/internal auth, the Hydra loopback guard, and stash-strength — yet control/gateway hard-bind `0.0.0.0` with no `--bind` (`control/main.rs:612`, `gateway:434`), and auth defaults `0.0.0.0:9092` (`auth/config.rs:27`). The worker, by contrast, defaults loopback and refuses non-loopback without `WORKER_KEY`.
- **Scenario:** `ZEROSHIP_DEV_INSECURE=1` left in a shared `.env` reaches staging → admin API + decrypted-env endpoints served unauthenticated on all interfaces.
- **Fix:** parse dev-insecure as `Option<bool>` (`--dev-insecure[=true|false]`) resolving CLI>env>default; add `--bind` defaulting to `127.0.0.1` on control/gateway/auth, or force loopback whenever `insecure_dev`.

### S2. DSN/secret env values are not hidden, and CLI structs derive `Debug` over raw secrets → credential leak. ✔ verified
[codex H3]
- No `hide_env_values` on any DB DSN: `control/main.rs:37` (`DATABASE_URL`), `gateway:79`, `worker:61`, `auth/config.rs:31` (`AUTH_DB_URL`). A DSN routinely embeds a password; clap can echo the env value in parse-error output.
- All four CLI structs are `#[derive(Debug, Parser)]` (`control:29`, `gateway:27`, `worker:23`, `auth/config.rs:11`) and hold raw `String` secrets before `SecretString` wrapping — any `{:?}` prints master/control/worker/stash keys in plaintext.
- **Fix:** add `hide_env_values = true` to every DSN field; drop `#[derive(Debug)]` from the CLI structs (or wrap secret fields so Debug redacts). NB: this corrects the security critic's over-claim that DSNs were already covered.

### S3. `WORKER_KEY` enforcement is one-sided. [security H1]
- Worker refuses a non-loopback bind without `WORKER_KEY` (`worker/main.rs:172`), but gateway only `warn!`s on an empty `worker_key` (`gateway/main.rs:233`) and **control has no check at all** (flows into `AppState.worker_key`, used to auth the admin log fan-out).
- **Scenario:** key set on workers, missing on gateway/control → cluster ships believing dispatch is authenticated; gateway sends `Authorization: Bearer ` (empty). Re-introduces the unauthenticated-execution risk the worker guard exists to prevent, from the caller side.
- **Fix:** `require_unless_dev("WORKER_KEY / --worker-key", …)` on gateway and control too.

### S4. Auth is not actually unified. [ALL FOUR critics — highest corroboration]
- Different flag/env: `--insecure-dev` / `AUTH_INSECURE_DEV` (clap-native bool) vs the others' `--dev-insecure` / `ZEROSHIP_DEV_INSECURE` (`env_is_exact "1"`). So `ZEROSHIP_DEV_INSECURE=1` silently no-ops for auth, and `AUTH_ALLOW_REMOTE_HYDRA_ADMIN=1` hard-errors (clap bool wants `true`/`false`) where `=1` works elsewhere.
- Second resolution path: `AuthConfig::resolve_file_overlay` (`auth/config.rs:306`) hand-rolls the same precedence the others get from `resolve_overlay_string`.
- Third stash validator: `auth/config.rs:346` uses `starts_with("dev-only-")` (a fragile prefix; a real key starting `dev-only-` is rejected) and a different sentinel than `core::config::validate_stash_key`'s exact-match.
- **Fix (pre-launch = rename/delete, no aliases):** put auth on `--dev-insecure`/`ZEROSHIP_DEV_INSECURE`, route it through the shared `resolve_overlay_string` + single `validate_stash_key(&str, bool)`, delete the auth copies.

### S5. (AD) Auto-discovery `try_exists` Err is fatal fleet-wide for a never-requested path, and the path is not symlink/owner/mode-hardened. ✔ (design tension confirmed)
[codex H6, security M5, workflow security-Info]
- `resolve_with_well_known` maps `try_exists() Err => ConfigError::Io => process::exit(1)` (`core/config.rs:262-273`). A `chmod 700 /etc/zeroship` (different owner) or a flaky NFS `/etc` makes **every** binary refuse to boot even though none was given `--config`.
- `try_exists()` + `read_to_string()` follows symlinks and checks no ownership/mode; "fixed path" ≠ "non-redirectable" if `/etc/zeroship` is writable by a non-root service account.
- **Fix:** gate severity on `discovered` — for the auto-discovered path, `try_exists` Err → `warn!` + defaults; keep fatal only for explicit `--config`. If hardening is wanted: secure-open (no-follow) + regular-file/owner/mode check.

### S6. `log_format` (and a bad `log_filter`) do not fail `--check-config` — silent degrade. [codex H7, design, security L1 — 3 sources]
- Invalid `--log-format=jsom` passes `--check-config` clean, prints the raw value, then falls back to `pretty` at boot (`observability.rs:93`). `resolve_log_filter` similarly swallows a bad filter to the default with only a pre-tracing `eprintln!`. Breaks the "nginx -t parity" claim (ADR) and injects pretty logs into a JSON pipeline silently.
- **Fix:** make observability values typed and **fatal** on invalid input (or at minimum have `--check-config` flag the fallback).

### S7. `deny_unknown_fields` is off → TOML typos silently become defaults. [codex H5, arch #4]
- `FileConfig`/`AuthSection`/`ObsSection` deliberately tolerate unknown fields "for future sections" (`core/config.rs:37`). That is a **back-compat behavior**, which the pre-launch no-back-compat stance forbids; a typo'd `hydra_pubic_url` is silently ignored.
- Also: the "no secrets in the file" property is convention-only (no field denies it structurally).
- **Fix:** add `#[serde(deny_unknown_fields)]`; optionally a unit test asserting no `FileConfig` field name matches a secret denylist.

### S8. Hydra-admin "loopback" validation resolves arbitrary hostnames; control has no remote-admin guard at all. [codex H9+H10]
- `auth/startup_validation.rs:49` resolves hostnames to decide loopback — DNS can change post-startup and the HTTP client may resolve differently (rebind/TOCTOU). Control consumes the privileged Hydra admin API (`control/main.rs:244`) with **no** equivalent guard.
- **Fix:** accept only literal loopback IPs / `localhost`; require explicit remote-admin opt-in otherwise; move the guard to `core` and apply it to every Hydra-admin consumer (control + auth).

---

## MEDIUM

### M1. `--check-config` has filesystem side effects on control. [codex M15, design, security M4 — 3 sources]
`create_dir_all` + write-probe + remove on `deploy_tmp_dir`, plus PAT signing-key load, all run **before** the `if cli.check_config` branch (`control/main.rs:296-316, 413-428` precede `:430`). A dry-run mutates the FS and exits 1 for writability reasons unrelated to config. **Fix:** move the check-config early-return ahead of all side-effecting preflight.

### M2. `--check-config` output is ad-hoc per binary, no machine-readable shape. [codex M14, arch #2, design]
Four copy-pasted `println!` blocks with disjoint field sets and conventions; no JSON mode. **Fix:** one structured emitter (`value` / `source` / `configured`) shared across binaries.

### M3. Key-strength semantics are inconsistent and partly leaky. [security M1+M2, codex H4]
Stash key checks raw UTF-8 byte len ≥32 (`core/config.rs:302`); master key checks *decoded* bytes ≥32 (`control/main.rs:187`); auth stash uses a prefix sentinel and **prints its dev default in `--help`** (`hide_env_values` doesn't hide `default_value`; `auth/config.rs:77`). Three answers to "is my key strong enough?" **Fix:** one decoded-entropy semantic; `hide_default_value = true` or empty-default+code-fallback for the auth stash key.

### M4. Invalid-state-representable encodings. [codex M16, arch #5]
- `trusted_oauth_clients = []` is indistinguishable from "unset → use compiled default" (`control/lib.rs:54`). Should be `Option<Vec<String>>` (absent = default; present-empty = no trusted clients).
- (AD) `ResolvedConfig { source: Option<PathBuf>, discovered: bool }` allows `{None, true}`. Prefer `enum ConfigSource { None, Explicit(PathBuf), Discovered(PathBuf) }` with one `Display` replacing `describe_source` + `log_overlay_source`.

### M5. Shared auth-domain defaults disagree across binaries + files. [codex M17, design D1]
Gateway defaults Hydra-public to compose-internal `http://hydra:4444` (`gateway/main.rs:23`); auth defaults to prod `https://auth.zeroship.ai` (`auth/config.rs:9`); control requires it outside dev. And `ops/zeroship.toml:6` ships `http://hydra:4444` while `ops/zeroship.example.toml:24` + the proposal show `https://auth.zeroship.ai` — an OIDC issuer-mismatch footgun in the auto-discovered dogfood stack. **Fix:** centralize domain defaults; require explicit Hydra URLs outside dev; reconcile the two ops files.

### M6. Dead `--auth-secret` knob still exposed. [codex M18, design]
Documented as legacy/removed from the auth path but still a CLI flag (`gateway/main.rs:52`). Pre-launch = delete it.

### M7. Worker-URL list parsed differently in `--check-config` vs runtime. [codex M19]
check-config filters empty entries for the count; runtime keeps them (`gateway/main.rs:300, 339`). **Fix:** parse once into `Vec<Url>` (reject empties) and reuse.

### M8. core/config.rs is a god-module; binaries duplicate the boot dance. [arch #1+#2+#6, codex M13, design D4]
~310 non-test lines spanning file schema, error type, two-tier resolution, env helpers, a clap `Args` group (drags `clap` into `core`), secret validation, source reporting, and `resolve_log_filter` (which `observability.rs` imports *backwards*); `load_overlay_or_exit` calls `process::exit` from a library. Each of the 4 mains re-types ~40 lines of load→resolve→tracing→check-config, with two different overlay-merge idioms. Adding one overlayable field touches ~5 places. **Fix:** split into `config::{file, resolve, env, secrets}`; move `ObservabilityFlags` next to `observability.rs`; return `Result` and let binaries own `exit`; a shared `bootstrap()` + `print_check_config()` (or a `derive`/macro) to collapse the per-binary dance.

---

## LOW / DOCS / OPS

- **O1. Compose inconsistencies** [codex H11, arch #9]: control points at `/data/bundles` but gateway/worker don't mount/pass the same blob store; **no `auth:` service** exists though gateway targets `http://auth:9092`; worker doesn't mount the overlay though it reads `[observability]`. (The auth gap is *why* S4's drift went unnoticed in the dogfood stack.)
- **O2. Runbooks don't actually boot** [codex H12, design D3]: `local-dev.md` uses `dev-master` (invalid key material) and omits required OIDC/auth fields; `local-dev.md:90` still passes `--auth-secret dev-jwt-secret` (perpetuates the JWT/auth-secret conflation the cleanup targeted); `docker-compose.md:36` wrongly says worker/auth are "not wired to this overlay" (they auto-discover, just don't mount).
- **O3.** Pre-tracing `eprintln!` (filter/overlay errors) is unstructured stderr even under `log_format=json` [design D6, security L1].
- **O4.** master-key validates length, not entropy; error text overclaims "random" (32 `A`s → 32 zero bytes accepted) [security L2].
- **O5. (AD)** Auto-discovery is default-on with no `--no-config` opt-out and only an `info!` signal — a stray `/etc/zeroship/zeroship.toml` silently reshapes every binary [design D2].
- **O6.** Dangling drift ID **D8**: the ADR mentions it; the proposal table skips D7→D9 [design, arch].
- **O7. (AD) ✅ already fixed:** proposal line 60 ↔ Open-Q #4 ("no auto-discovery" vs "implemented") contradiction — the auto-discovery workflow's fix phase already corrected line 60 and the ADR Decision body [codex #20, design, arch, workflow Medium].

---

## What's actually solid (verified, for fair calibration)

1. Secrets are **structurally absent** from the TOML overlay — no secret field exists, so a secret in TOML is simply ignored (not just discouraged).
2. Auto-discovery is a fixed `const` path probed via `Path::new(SYSTEM_CONFIG_PATH)` — **no env/CWD/$HOME can redirect it** (the only redirection is explicit `--config`/`ZEROSHIP_CONFIG`).
3. The discovery invariant holds and is hermetically tested: explicit-missing → hard error, discovered-missing → default, discovered-but-malformed → hard error. (The rough edge is the `try_exists` Err severity, S5.)
4. `SecretString` is a real footgun-resistant wrapper (no Display/Serialize/Deref, redacting Debug, zeroize-on-drop).
5. `--check-config` prints **no secret value** in any of the four binaries (booleans/counts only).
6. Insecure-mode env truthiness **fails closed** against typos (`=true`/`=yes` ignored) — the only enable footgun is a literal leftover `=1` (S1).
7. The D1–D9 flag unification (blob-store, worker-threads, hydra naming, typed numerics) is real and regression-tested for the three legacy binaries.

---

## Suggested fix ordering

1. **Security tier (S1–S8):** dev-insecure precedence + loopback bind; hide DSN env values + drop Debug; symmetric WORKER_KEY guard; unify auth; harden/soften auto-discovery `try_exists`; typed-fatal observability; `deny_unknown_fields`; literal-loopback Hydra guard on all consumers.
2. **Correctness/ergonomics (M1–M7):** read-only `--check-config`; structured check-config output; one key-strength semantic; `Option<Vec>`/`ConfigSource` enums; reconcile domain defaults + ops files; delete `--auth-secret`; single worker-URL parse.
3. **Architecture (M8):** split the god-module, de-duplicate the boot dance, return `Result` from the loader. (Largest scope; a separate decision.)
4. **Docs/ops (O1–O6):** fix compose blob-store/auth-service/worker-overlay; make runbooks actually boot; structured pre-tracing errors; entropy wording; `--no-config`; resolve D8.
