# Architecture review — 2026-05-24 round 5

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `15b4f9a8`
**Lens**: architecture — structural decay; B19 seam audit
**Prior**: `…-architecture-2026-05-24-r4.md` (HEAD `a9e568a2`)

## Summary

7 findings (2 critical, 3 important, 2 minor). r4 closures: **R3-Q3 (fixture
bump) and R4-T1 (shellcheck gate) cleanly closed; all r4 structural
criticals (C1 `AppStateBuilder`, C2 r3 carry-overs) still OPEN**. B19
shipped the symptom fix but introduced two new seams — `nomad_ch_handle()`
and a 3-impl `Backend::register_restored` switch — that worsen the
"enum-masquerading-as-trait" pattern r3-A1 flagged. The RAII gap R4-A2
identified is NOT addressed: B19 papers over the missing state-map insert
by adding a second insert point (controller-side, post-`wait_for_livez`),
but state-map removal still happens up-front in `stop_inner` while
vm_index release straddles 60-120s of fence work.

## CRITICAL

**C1. `backend/mod.rs:175-180,449-505` — B19 worsened R3-A1 by adding a
THIRD nomad-ch-only method + asymmetric `Arc` wrap on the enum.**
The `Backend` enum now wraps `NomadCh(Arc<NomadCHBackend>)` while
`Docker`/`K8s` stay by-value (:175-180). The asymmetry exists solely so
`nomad_ch_handle()` (:467-474) can hand out a clone of the inner `Arc`
to `RealRestoreBackend::with_nomad_handle`. `register_restored` (:487-505)
is now the 5th method with `Err("backend X doesn't support …")` arms for
Docker/K8s (joining `lookup_source_vm_ops`, `teardown_source_for_snapshot`,
`restore_from_sealed`, `restore_from_pg_and_sealed`). Each B-series fix
that needs to reach into nomad-ch now has the same forced choice: extend
the enum (and the Err-arms grow) OR add a new `*_handle()` escape hatch
returning the inner `Arc`. Two precedents now exist; the third will
follow. The structural fix is unchanged from r3-A1: split
`SnapshotCapableBackend: Backend` trait, make the controller hold
`Arc<dyn SnapshotCapableBackend>` at the call sites that need the
surface, delete the `Docker(_) | K8s(_) => Err(...)` boilerplate.

**C2. R4-A2 NOT closed by B19 — state-map insert is now at TWO sites
with no unified guard.** `nomad_ch.rs:1650-1681` (`register_restored`)
inserts post-`wait_for_livez` from the controller; `nomad_ch.rs:977-990`
(`stop_inner`) still `remove()`s up-front before the 30+120s fence; the
vm_index release lives at the very end (r4-A2 line refs preserved). B19
added a NEW insert point without consolidating the existing two
(`create` + `restore_from_pg_and_sealed`). The structural shape r4
asked for — `LeasedVmSlot` RAII owning both the OccupiedEntry and the
allocator reservation — is unaddressed; the wake path now has FOUR
ways to enter the state map (`create`, `restore_from_pg_and_sealed` for
restart-restore, `register_restored` for wake-restore, plus the
fail-path Vacant/Occupied disagreement at :1676-1679 which returns Err
on Occupied). Each entry path has its own pre/post-condition discipline.

## IMPORTANT

**I1. `restore_handler.rs:162-170` — `register_restored` trait default
impl `Ok(())` is a foot-gun.** The default impl returns `Ok(())` so
`StubRestoreBackend` compiles unchanged (:155-161 doc). But the prod
wiring at `lib.rs:647-664` warns and proceeds when `nomad_ch_handle()`
returns `None` — and the `RealRestoreBackend::register_restored` at
:1018-1047 returns Err if the handle is unset (:1025-1028, explicit
guard). So the test-default disagrees with the prod-default: prod fails
loudly, tests silently no-op. A test that drove the trait through a
custom impl forgetting to override `register_restored` would pass while
the prod path 500'd. Either remove the default impl (force every
implementor to make the choice) or return `Err("not implemented")`
matching the prod failure mode.

**I2. `restore_handler.rs:1050-1056` — copy-pasted false comment on
`build_restore_nomad_job_json` survives B19.** Verbatim from r4-I4:
"we don't share the helper because the restore path doesn't have a
`user_id`/`project_id` to plumb through Meta" — `user_id` is now
plumbed at :1063 of the same function signature, contradicting the
adjacent comment. r3-C3 / r4-I4 carried; the false comment is a beacon
for the next fixer that "this duplicate is justified". The 119-line
duplicate (:1057-1175 vs `nomad_ch::build_nomad_job_json`) plus the
in-comment alibi is the worst kind of code rot — a wrong comment is
worse than no comment.

**I3. `lib.rs:617-664` — B19 wiring is now 47 lines of warn-and-fall-
through for nullable backend handles.** Two consecutive `match
Option<_>` blocks each with an `Some => with_*(self, h)` arm and a
warn-log `None` arm (B18's `shared_allocator` then B19's `nomad_handle`).
Each missing handle surfaces as a runtime warn + degraded behaviour
later; neither aborts boot. With the next B-series likely needing
another `Backend::*_handle()` accessor, this block will hit ~70 lines
of permutation. An `Arc<dyn SnapshotCapableBackend>` constructor that
takes the trio of dependencies and validates them at boot would collapse
this to one line + one Err — same shape as `Backend::from_config`.

## MINOR

**M1. `nomad_ch.rs:1676-1679` — `register_restored` Occupied-Entry
clobber check is good defensive code but the error message is misleading.**
The Err text "would clobber live record; refusing" implies a concurrent
write; the actual cause is a controller-restart restart-restore path
having already populated the entry via `restore_from_pg_and_sealed`.
Should reference both code paths so an on-call following the log knows
to check the restart-restore flow, not chase a phantom race.

**M2. R3-Q3 + R4-T1 cleanly closed — confirmation.** `28f60d73` updates
7 fixture sites (`backend/docker.rs:832`, `backend/nomad_ch.rs:3449`,
`config.rs:744,652`, `lib.rs:1267,1399`, etc.) from 60→120. `4e6c70c1`
adds `crates/sandbox/scripts/lint.sh` + `tests/scripts_lint.rs` (108
LOC integration test, `#[ignore]`-gated `_required` variant for CI).
Both are uncomplicated wins. The shellcheck test only opt-in-gates
(:25-30 in the brief) — CI flip from advisory→hard remains to land.

## r4 status

| r4 finding | Status at `15b4f9a8` |
|---|---|
| C1 AppStateBuilder typed-state | **OPEN** (still 8 `with_*` + `new_fixture` on 2 types) |
| C2 r3 carry-overs (enum/sweep/restore_handler) | **OPEN + WORSENED** (B19 adds 5th Err-arm method + asymmetric `Arc` wrap) |
| I1 `new_fixture` on 2 prod types | **OPEN** |
| I2 `pub fn token()` asymmetric shielding | **OPEN** |
| I3 state-map + vm_index split lifetime | **OPEN + WORSENED** (B19 adds 3rd insert path; no RAII) |
| I4 restore_handler 2nd backend impl | **OPEN** (false comment survives) |
| R3-Q2 infallible `Result` builder | **OPEN** (9 `with_*` now; only `with_persistence` is `Result`) |
| R3-Q3 stale 60 fixtures | **CLOSED** (`28f60d73`) |
| R4-T1 shellcheck gate | **CLOSED** (`4e6c70c1`; `_required` variant `#[ignore]`d) |

**Net**: 2/9 r4 findings closed. B19 added 158 LOC to `restore_handler.rs`
+ 212 LOC to `nomad_ch.rs` while the underlying structural design holes
(`SnapshotCapableBackend` split, `LeasedVmSlot` RAII, `AppStateBuilder`
typed-state) accumulated zero closures across 3 rounds. The "shape the
fix takes" continues to be "add another method to `Backend` + another
`with_*` + another null-check in the wiring block" rather than "lift
the abstraction". Next cycle should pick one of r3-A1 / r4-A1 / r4-A2
and land it before the next B/A-series fix lands.
