# Architecture review — 2026-05-24 round 4

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `a9e568a2`
**Lens**: architecture — structural decay
**Prior**: `…-architecture-2026-05-24-r3.md` (HEAD `4340e3b5`)

## Summary

8 findings (2 critical, 4 important, 2 minor). r3 closures: **only R3-Q1**
(`a11ccb2d` truthful `attempted` in `sweep.rs`). r3's three structural
criticals (C1 backend enum, C2 sweep duplicate, C3 restore_handler) are
all still open. New angles this round: a `new_fixture()` + `with_*`
builder family now spans two production types (smell), and the credential
shielding pattern landed without resolving the layering issue underneath it.

## CRITICAL

**C1. `lib.rs:50-407` — Builder family is a typed-state `AppStateBuilder` in
denial.**
A5/A6/A6b/A7 stamped `with_admin_token`, `with_persistence`, `with_config`,
`with_database`, `with_snapshot_store`, `with_ch_remote`,
`with_restore_backend` onto `AppState` itself. `with_admin_token` returns
`Result<Self, String>` (:216-233); `with_persistence` returns infallible
`Result<Self, String>` (:272-278, R3-Q2 still open); the five A6b builders
return `Self` (:315-377). Three return-type shapes for one pattern, on a
struct (`AppState`) whose `from_config` constructor is 290 LOC
(:416-716) and whose `new_fixture` is a 14-field shadow of it (:392-406).
A proper `AppStateBuilder { config, backend, admin_token: Option<…>, … }`
with one fallible `build()` would collapse the eight setters, eliminate
`new_fixture`'s second-construction-path drift risk, and stop the next
credential field (the only ones unshielded today are `sandboxes`,
`mint_rate_limiter`, `shutdown`) from triggering an A8.
Fix: lift the builder into a `pub struct AppStateBuilder` in
`lib.rs`; `AppState`'s fields stay `pub(crate)`; `from_config` becomes
`AppStateBuilder::from_env().build().await`; `new_fixture` becomes
`AppStateBuilder::default().build_sync_for_tests()`.

**C2. r3 C1/C2/C3 all still open; **only** R3-Q1 closed.**
Between `4340e3b5` (r3 HEAD) and `a9e568a2` the only architectural commits
are `a11ccb2d` (sweep attempted-list — closes R3-Q1) and `a9e568a2` (A7
`SandboxConfig.token` — surface, not structure). r3 critical themes
unchanged: `backend/mod.rs:359-451` still has four nomad-ch-only
methods with `Err`-arms for Docker/K8s; `sweep.rs:283-411` still mirrors
`admin_handlers.rs:1102-1234` byte-for-byte; `restore_handler.rs:842-969`
is still a second `build_nomad_job_json` (the in-comment justification at
:838 — "no user_id/project_id" — remains false; the helper takes
`user_id` at :849). Two consecutive rounds with zero structural closes
indicates the credential-shielding work is crowding out load-bearing
refactors.

## IMPORTANT

**I1. `lib.rs:392-406` + `config.rs:619-651` — `new_fixture()` is on TWO
production types now.**
A7 propagated the pattern: `AppState::new_fixture` (r3 catalogued) and
`SandboxConfig::new_fixture` (new this cycle). Both are `pub` on prod
types whose only docstring caller is "out-of-crate integration tests"
(:379-389; :604-618). The "production code uses `from_env`/`from_config`"
disclaimer in each docstring is the smell — a `#[cfg(test)]`-gated module
+ a `pub mod test_support` re-export, or a feature-gated builder, achieves
the test-reach without permanently widening the prod API surface. Today's
shape leaks an inert no-network `nomad-ch` config + admin-disabled state
into every downstream consumer's autocomplete.
Fix: gate both behind `#[cfg(any(test, feature = "test-support"))]`; the
two out-of-crate test crates (`crates/sandbox/tests/*`) flip the feature.

**I2. `config.rs:577-579` — `pub fn token(&self)` is inconsistent shielding.**
A7 made `token` `pub(crate)` and added a `pub fn token(&self) -> &ApiToken`
accessor. The other nine `SandboxConfig` fields (`port`, `backend`,
`image`, `workspace_root`, `network`, `memory_mb`, `cpus`,
`idle_timeout_secs`, `max_lifetime_secs`, `auto_pull`) remain `pub`. The
docstring (:564-572) defends the asymmetry — only the main binary needs
to read the token across the crate boundary — but the precedent is
exactly what landed five A6b builders one cycle ago. If `token` deserves
shielding, so does `nomad_ch.nomad_addr` (an attacker pivot), and the
crate now has a half-shielded `SandboxConfig` whose state space is
"all-pub except token". Either go full `pub(crate)` + 10 accessors, or
admit the shielding is a `pub(crate)`-by-construction story (i.e.
`AppStateBuilder` takes ownership of an opaque `SandboxConfig` and the
inner fields don't need to be `pub` at all).

**I3. `backend/nomad_ch.rs:944-952` + `:1087-1091` — state-map removal and
vm_index release are split by 60-120 s of async work (B18 surface area).**
`stop_inner` removes the sandbox from `state` HashMap up-front (:944-952)
and only releases `vm_index_allocator` after the host fence at
`:1087-1091` — separated by `wait_for_job_gone` (`:1001-1006`, 30 s) +
`wait_for_agent_silent` (`:1051-1054`, default 120 s). The two
locked structures are touched at different points in the same critical
section without a unified guard, which is the architectural shape B18 is
hunting (concurrency-r3). Even after B18 closes (probably by stamping the
new pubkey unconditionally in `/sbin/init`), the underlying coupling
remains: a single `SandboxRecord` lifetime conceptually spans both
structures but no type encodes "owns vm_index and state-map slot
together". The leak comment at `:1097-1124` confirms the asymmetry: an
index can leak independently of the map slot, and vice versa.
Fix: a `SandboxSlot` RAII guard that owns both the vm_index reservation
and the `HashMap::OccupiedEntry`, released atomically on Drop. State-map
removal moves AFTER fence, eliminating the window where a stopped
sandbox has a live index but no map entry.

**I4. `restore_handler.rs:842-969` is a non-trivial duplicate of
`backend/nomad_ch.rs::build_nomad_job_json`.**
r3-C3 re-stated; flagging here that the false comment at `:838-841`
("no user_id/project_id to plumb through Meta") is now contradicted by
`:849, :876-877, :892-895` where `user_id` IS plumbed through both Meta
AND the `user_home_dir` path. The justification rotted; the duplicate
hasn't been touched. Each Nomad-jobspec field added to cold-boot
(`MemoryMaxMB`, `KillTimeout`, `RestartPolicy`) is now a "remember to
mirror in restore" item — bug-#9's MemoryMaxMB=2× lives twice
(`:954-958` on restore + the cold-boot mirror).

## MINOR

**M1. Pre-existing TODO debt is small but pristine — three TODOs, all
load-bearing.**
`db.rs:494-507` (round-1 fixer, IMPORTANT #10, `2380605e`+ era — ~4
months old per blame): per-call pool open round-trips a pg connect on
every query. Hot path, documented-not-fixed. `snapshot_store_gcs.rs:817`
(May 2026, GCS PR): missing retry loop on L2 upload. `backend/k8s.rs:495`:
restore_from_sealed back-reference. None younger than the snapshot-restore
branch itself; none rotting >12 months. Cleanest TODO surface seen in
3 rounds — but `db.rs:494` should either land or migrate to
`deferred.md` so it doesn't fade behind newer items.

**M2. `lib.rs:288-296` — `#[allow(dead_code)]` on the A6 `persist()`
accessor is a tell.**
Comment (:286-292) says "today the in-crate uses access `self.persist`
directly (they were written before this helper landed)". An accessor
that nothing calls is the wrong fix for "we want a single legal read
path"; either migrate the four call sites or delete the accessor. The
allow-dead-code attribute is a permanent reminder that the pattern is
half-applied.

## r3 status

| r3 finding | Status at `a9e568a2` |
|---|---|
| C1 Backend enum 4× Err-arm | **OPEN** (C2 here) |
| C2 sweep duplicates admin | **OPEN** (C2 here) |
| C3 restore_handler 2nd backend | **OPEN** (I4 here) |
| I1 SnapshotWiring collapse | **OPEN** + worsened (A7 added with_token) |
| I2 wrapper sed JSON rewrite | **OPEN** (W1 in deferred) |
| I3 StopDisposition enum | **OPEN** (no movement) |
| R3-Q1 sweep attempted lie | **CLOSED** (`a11ccb2d`) |
| R3-Q2 infallible Result builders | **OPEN** (A7 followed pattern, but `with_persistence` still `Result<Self, String>`) |

**Net**: 1/8 r3 findings closed. Two consecutive rounds where credential
surface-shielding (A5→A6→A6b→A7) crowded out structural refactors. C1
(`AppStateBuilder`) and a SnapshotCapableBackend trait should land before
the next A-series accretes.
