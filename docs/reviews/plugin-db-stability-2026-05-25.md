# plugin-db stability review notes — 2026-05-25

This worktree did not contain the referenced review file, so this note records the defer decision made while landing the stability fixes on `plugin-db/stability-fixes`.

## Deferred

### IMPORTANT 2 — `LockScope::GlobalApp` on SQLite is backend-local, not isolate-global

Status: deferred.

Reason: the current SQLite advisory lock registry is instantiated per `SqliteBackend` as `Rc<InProcessLockRegistry>` backed by `RefCell<HashMap<...>>` in [crates/plugin-db/src/backend/sqlite/lock.rs](/home/ruiyang/Projects/appbase/.claude/worktrees/stability-fixes/crates/plugin-db/src/backend/sqlite/lock.rs:39) and wired from [crates/plugin-db/src/backend/sqlite/mod.rs](/home/ruiyang/Projects/appbase/.claude/worktrees/stability-fixes/crates/plugin-db/src/backend/sqlite/mod.rs:426). That means two backend instances can each believe they hold the same `GlobalApp` lock. Making this truly isolate-global requires replacing the backend-local `Rc<RefCell<...>>` registry with a process-shared registry keyed by database identity, plus thread-safe ownership semantics across worker threads. That is a larger redesign than the bounded cancellation/guard/cache fixes in this pass.

Why it is acceptable to defer in this pass: SQLite remains the dev/small-scale backend, and the higher-priority stability failures here were concrete leak/cancellation bugs that could wedge a live backend instance. This lock-visibility issue is real, but it is a coordination-model redesign rather than a small correctness patch.
