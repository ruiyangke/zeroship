# Sandbox snapshot/restore — API-surface review (2026-05-24 r3)

Branch `feat/sandbox-snapshot-restore` @ `03d15012`. Read-only delta over r2
(`3b888a2a`). Verifies r2 closures, A6b (`2380605e`), C2 (`78320b56`), B17
wrapper (`dec489a1`).

## Closure status carried from r2

- **A5 (`admin_token`)** — closed in r1 land, still clean.
- **A6 (`persist`)** — closed in r2 land, still clean.
- **A6b** — **CLOSED** at `2380605e`: 5 fields (`config`, `database`,
  `snapshot_store`, `ch_remote`, `restore_backend`) narrowed to
  `pub(crate)` (`lib.rs:61,83,157-159`); 5 setter tests pin replace
  semantics (`lib.rs:1683-1803`); `Database::for_setter_test_only`
  correctly gated `#[cfg(test)] pub(crate)` (`db.rs:458-471`).
- **A4 (error envelope §10.0)** — **STILL OPEN** for the 4th round. No
  `error_envelope.rs` helper exists. Inventory below.
- **`Result<_, String>` count** — now **153** across 15 files (down from
  167 in r2 and 164 in r1). Direction finally correct. Worst offenders:
  `backend/nomad_ch.rs` (35), `backend/k8s.rs` (30), `backend/docker.rs`
  (18), `restore_handler.rs` (17), `snapshot_handler.rs` (10).
- **B17 wrapper fix** (`nomad-vm-wrapper.sh:366-419`) uses only the CH
  API socket (`ch-remote ping`/`resume`) — **no new controller HTTP
  route**. Zero Rust API-surface impact. Verified.

## New findings (post-A6b)

### CRITICAL

1. **A4 still bleeding — 18 non-compliant `"error"` sites across 5
   files.** Spec (§10.0) is `{"error":"<kind>","message":"<human>",...}`.
   Current state:
   - `admin_handlers.rs:181,196` — bare `{"error":"<prose>"}` (no kind /
     no message). Used by every non-snapshot admin endpoint.
   - `handlers.rs:22,43` — verbatim duplicate of the above pattern.
   - `preview.rs:817,824,831,837` — emit `{"error":<code>,"code":<code>}`
     (duplicate code, no message field).
   - `preview_share_handlers.rs:100,107,113,212` — same shape as
     preview.rs (`code` field, no `message`).
   - Only **8 spec-compliant** sites: `admin_handlers.rs:1043-1098,1175`
     (snapshot/restore typed map). Ratio 8 compliant / 26 total = 31 %.

   The 4-round gap is procedural — every other r1/r2 finding has landed.
   A typed `ErrorEnvelope { error, message, code?, fields: HashMap }`
   helper in `error_envelope.rs` + sed-friendly migration of the 4
   `unauthorized()/err()/uniform_*()/err_with_code()` helpers would
   collapse the 18 sites in one PR. Lock with a per-handler
   `assert_envelope!(resp)` unit test.

### MAJOR

2. **`IdleSnapshotter` trait + `ControllerIdleSnapshotter` struct still
   `pub`** — `sweep.rs:253,296`. r2 finding #2/#3 unchanged. Single
   in-crate construction site (`lib.rs:711`); one production impl, one
   `#[doc(hidden)] pub` test impl. Should be `pub(crate) trait …` /
   `pub(crate) struct …` with `pub(crate) fn new`. Adding a method to
   the trait (e.g. `snapshot_many`, `shutdown`) is currently a SemVer
   break despite zero out-of-crate consumers. T10 (deferred) wants this
   refactored to drop `Arc<AppState>` anyway — sealing first makes that
   safe.

3. **`AppState::config()` accessor has zero out-of-crate callers** —
   `lib.rs:342-344`. `database()` (`lib.rs:334`) is justified —
   `tests/sandbox_pg_e2e.rs:2709` uses it. `config()` is not used
   anywhere outside the crate (grep finds zero hits). The accessor was
   added speculatively. Either narrow to `pub(crate) fn config` or
   remove it. Same applies, less critically, to `persist()` at
   `lib.rs:294` which is already `pub(crate)` and `#[allow(dead_code)]`.

4. **A7 — `SandboxConfig.token: ApiToken` still `pub`** — `config.rs:60`
   (brief said line 19; that's `ApiToken` struct, not the field). The
   creator-side bearer is a `pub` field on a `Clone, Debug` struct,
   meaning every `AppState::with_config(attacker_cfg)` (the new A6b
   builder!) can plant an attacker-controlled bearer. A6b restricted
   the `AppState.config` field but routed all writes through
   `with_config(cfg)` — and `cfg.token` is still externally writable.
   Action: same shape as A5 — `pub(crate) token: ApiToken` on
   `SandboxConfig` + a `with_token(...)` builder rejecting empty
   strings; migrate the parser site at `config.rs::from_env` (in-crate)
   to the builder. Three other field-write paths (`SANDBOX_PORT`,
   `SANDBOX_ALLOW_NO_AUTH`, etc.) are non-credential and can stay `pub`
   for now.

### MINOR

5. **17 `#[doc(hidden)] pub` items unchanged from r2** — `metrics.rs`
   carries 11 (`metrics.rs` is the worst); `db.rs` 2; one each in
   `restore_handler.rs`, `snapshot_handler.rs`, `restore.rs`, `sweep.rs`.
   `#[doc(hidden)] pub` hides from rustdoc but stays in SemVer surface
   — `metrics.rs` cluster should be `pub(crate)`.

6. **`with_persistence` / `with_config` infallible-Result smell
   confirmed** (r3 code-quality #Q2 cross-link). `with_persistence`
   returns `Result<Self, String>` but cannot fail (`lib.rs:272-278`).
   `with_config`, `with_database`, `with_snapshot_store`,
   `with_ch_remote`, `with_restore_backend` all return `Self` and the
   delta forces in-crate test code to `.expect()` on the persist
   builder for no reason. Either make them all infallible (current
   majority shape) or all `Result` (future-proof shape) — the mix is
   the smell.

7. **`admin_token()` `&str` accessor still leaks the secret** —
   `lib.rs:241-243`. r2 minor #5 unchanged. Returning `&Zeroizing<String>`
   would force callers to use the scrubbing wrapper; the current `&str`
   permits trivial `.to_string()` escape.

## Summary

- **Critical:** 1 new (A4 still open with 18 sites quantified) + **1
  carried** (A7 — `SandboxConfig.token` last `pub` credential field;
  was listed in deferred but unchanged in code) = **2**
- **Major:** 3 — sealed-trait gap on `IdleSnapshotter`, speculative
  `pub fn config()`, `Result`-smell on the new builders
- **Minor:** 2 — `doc(hidden)` cluster on `metrics.rs`, `&str` leak on
  `admin_token()`

A6b cleanly closed 5 fields + 5 builder tests + the test-only Database
constructor. A4 is the last r1 finding still open — 4 rounds, 18 sites,
no `ErrorEnvelope` helper exists. A7 (`SandboxConfig.token`) is the
last credential-bearing `pub` field; until it lands, A6b's
`with_config` builder is a leaky abstraction.
