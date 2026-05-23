# Sandbox Snapshot/Restore — Code-Quality Review (Round 6)

Date: 2026-05-24
Branch: `feat/sandbox-snapshot-restore` @ `1066a319`
Scope: `crates/sandbox/**`
Prior rounds: r1 (17), r2 (7), r3 (8), r4 (8), r5 (8).

## Trend (`crates/sandbox/src/`)

| Pattern | r4 | r5 stated | r5 re-measured | **r6 HEAD** | Δ r5→r6 |
|---|---|---|---|---|---|
| `Result<_, String>` | 155 | 159 | 159 | **157** | **−2** |
| `Duration::from_secs` | 62 | 62 | 62 | **62** | 0 |
| `.unwrap()` | 255 | 267 | **277** (recount) | **277** | 0 |
| `.lock().unwrap()` | 19 | 25 | 25 | **25** | 0 |
| `error_response(…)` | 0 | 22 | 22 | **23** | +1 |
| Distinct error codes (err+err_safe+error_response+envelope) | – | 38 (incl tests) | 20 (src) | **43** (src) | **+23** |
| `err_safe()` sites | 0 | 0 | 0 | **41** | +41 |
| `#[allow(dead_code)]` / `_anchor` | 7/3 | 7/3 | 7/3 | **7/3** | 0 |
| Top fn LOC | 352 | 352 | 352 | **352** (`lib.rs:417 from_config`) | 0 |

**r5 number corrections.** r5 stated `.unwrap()` = 267; re-measured
at `0aa93a0f` is **277** (r5 under-counted by 10; trend +12 is real,
absolute is not). r5's "38 distinct codes" included
`crates/sandbox/tests/`; src-only at r5 baseline was 20.

**Two Rust commits between r5 baseline (`0aa93a0f`) and HEAD**
(r5 said one): `ac6a6bf2` (R3-Q2 — `with_persistence → Self`) and
`4fd92bef` (S4 — admin sanitization). S4 landed mid-r5 after the
security review was written.

## Findings

### MAJOR

1. **S4 sanitized admin only — `handlers.rs` still leaks raw `{e}`
   in 3 sites.** `admin_handlers.rs` got 41 `err_safe` conversions
   (helper at `admin_handlers.rs:227-244`). But `handlers.rs:670`,
   `:821`, `:837` still pass `format!("backend.stop: {e}")` /
   `backend.exec: {e}` / `backend.file_tree: {e}` straight into
   `err(500, …)`. Same threat model (backend stderr renders Nomad
   alloc paths + addresses), different file, missed. S4 closes the
   admin-token surface but leaves the creator-facing surface.

2. **A4 error-code vocabulary exploded 20 → 43 (+23) via S4.**
   Every `err_safe(…)` callsite minted a *new specific* code:
   `gdpr_delete_events_failed`, `gdpr_tombstone_failed`,
   `export_sandboxes_failed`, `pg_tx_isolation_failed`, etc.
   (`admin_handlers.rs:728-1015`). 16 of the 23 new codes have one
   call site each. r5 flagged "3 variants for not-found" as a smell;
   the same disease metastasized — 12 distinct pg/gdpr internal-error
   codes where one or two stable codes (`pg_failed` / `pg_gdpr_failed`)
   plus the existing `tracing::error!(operation=…)` carry the same
   information. The wire-shape tests (`admin_handlers.rs:1612+`)
   pin individual code strings, locking the explosion in.

3. **`register_restored` trait default impl still silent `Ok(())`**
   (`restore_handler.rs:162-170`, flagged r5). Unchanged. The
   doc-comment at `:155-161` now explicitly documents the silent
   no-op as *feature*. Doc-as-feature framing makes it less likely
   to ever get fixed.

### MINOR

4. **`nomad-vm-wrapper.sh:388-419` un-reaped subshell — fourth round,
   un-fixed.** The `( … ) &` block at `:419` has no captured PID, no
   `wait`, no entry in the EXIT trap (which handles only `$CH_PID`
   at `:288`). Every wake leaves a defunct shell entry for ~3s; at
   c=4 with 9-wake bursts that's 36 defuncts per burst. **Three-line
   fix**: capture `RESUME_PID=$!`, add `wait "$RESUME_PID" 2>/dev/null
   || true` to cleanup. Not gated on R3-A3 Rustification — pure
   shell hygiene. Carrying it across r3/r4/r5/r6 without picking it
   is review-velocity smell.

5. **R3-Q2 (`ac6a6bf2`) is clean.** `with_persistence` returns `Self`
   (`lib.rs:262-281`); 3 in-crate `.expect("infallible operation")`
   removed (`lib.rs:1607+`). Doc-comment honestly attributes the
   pivot to r3/r4/r5. `Result<_, String>` count dropped 159→157.
   Net positive, narrowly scoped. Closes R3-Q2.

6. **`Persistence::unseal` properly typed.** `persist.rs:677` returns
   `std::io::Result<SealedAuth>`; spawn_blocking panic path uses
   `io::Error::other(...)` (`:683`). No `Result<_, String>`
   propagation from this new surface.

7. **`from_config` unchanged 352 LOC; `do_restore_inner` 170 LOC.**
   `lookup_source_vm_ops` (r1's 410-LOC outlier) is now 94 LOC at
   `backend/nomad_ch.rs:1709-1802`. No new `#[allow(dead_code)]` or
   `_anchor`.

## Score: 77/100 (+1 from r5)

- **Correctness 80** (=) — no Rust behaviour change vs r5; trait
  silent-no-op carryover keeps this stuck.
- **Performance 82** (=) — no perf-affecting changes since r5.
- **Security 80** (+2) — S4 closed 41 admin sites; creator-facing
  `handlers.rs` leak (3 sites) drags this back from full +5.
- **API Design 64** (−4) — code-string vocabulary 20→43; 16 new
  codes with one call site each make the stable contract noisier,
  not finer.
- **Rust Idioms 76** (+2) — R3-Q2 removed an infallible
  `Result<_, String>` and 3 `.expect()` sites; counts moved in the
  right direction.
