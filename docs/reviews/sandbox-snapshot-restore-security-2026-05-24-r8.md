# Sandbox Snapshot/Restore — Security Review r8 (2026-05-24)

Branch `feat/sandbox-snapshot-restore` @ `f2d89b61`. Focus: R7-S1 closure
(`e95baa89`), R7-S2 sentinel removal, standing items (A1, W1).

## R7-S1 closure verdict — **SOUND, with one deployment gap**

`crates/sandbox-agent/src/handlers.rs:704-851` (`clock_resync`):

1. Signature verified via `verify_signed_skew_bypass`
   (`handlers.rs:711`) **before** body parse; the body-hash slot covers
   `sandbox_id` + `ts` + `challenge` (`sig.rs:118-134`).
2. `ResyncBody` is strict `serde::Deserialize` — missing fields → 400
   (pinned by `clock_resync_rejects_missing_sandbox_id` at
   `handlers.rs:1758`); wrong types also 400.
3. Oversize-body DoS bounded by route inheriting the 4 KiB
   `small_limit` `PayloadConfig` (`main.rs:148,166,197`); ntex 413s
   pre-handler.
4. `challenge` shape gate (`handlers.rs:761-776`) enforces exactly 64
   lowercase hex chars **before** LRU touch — no cache pollution via
   junk strings even with a valid signature.
5. LRU contains-check is done BEFORE `cache.put`; lock is dropped
   before `settimeofday(2)` (`handlers.rs:796-809`).

**LRU capacity 4 — not exploitable.** Five captured replays carry 5
distinct controller-minted challenges (`restore_handler.rs:1514` reads
32 bytes from `/dev/urandom` per call). Even if C_N is evicted from the
challenge LRU, the verifier's nonce LRU
(`sig.rs:498-503`, cap 10 000, TTL 30 s) lives in the snapshotted VM
heap and still rejects the outer nonce. Defence-in-depth holds.

`sandbox_id` comparison at `handlers.rs:743` is plain `!=` on
`String`/`&str` (timing-variable). Not material: signature gate fires
first, and audit log leaks only lengths (fixed for UUIDs).

R7-S2: sentinel `http://127.0.0.1:0` is gone from the trait
(`restore_handler.rs:187` — `derive_agent_url` is now a required
method; default removed per the docstring at `:180-186`). Real impl at
`:1075` returns the production agent URL; stub at `:733-739` uses a
deterministic loopback. No callsite uses the old sentinel.

## Findings

### CRITICAL

1. **`crates/sandbox-agent/src/main.rs:97-102` — fail-closed bind to
   `SANDBOX_AGENT_SANDBOX_ID` reads env or `/run/keys/sandbox-id`, but
   `crates/sandbox/scripts/nomad-vm-wrapper.sh` writes neither.** Grep
   on the worktree finds zero wrapper-side writers. The deferred file
   even flags this in the R7-S1 closure note ("wrapper
   SANDBOX_AGENT_SANDBOX_ID env injection needed for cluster smoke").
   Fail mode is correct (boot exits 1 → controller surfaces livez
   timeout), but every cluster wake on this branch is broken until
   the wrapper lands the env injection.

2. **`crates/sandbox/src/lib.rs:316-345` — A1 still open.** Prod build
   never wraps the inner store in `AeadSnapshotStore`;
   `snapshot_handler.rs:358` stamps `snapshot_aead_dek_id="v1"` while
   guest RAM ships to GCS in plaintext. R6 AppStateBuilder proposal
   0/3.

3. **`crates/sandbox/scripts/nomad-vm-wrapper.sh:367` — W1 still
   open.** Unanchored `sed -i -E "s#…#${NOMAD_TASK_DIR}#g"` on
   attacker-influenceable `config.json`. Dormant only because A1
   keeps the artifact locally cached.

### MAJOR

4. **`crates/sandbox-agent/src/handlers.rs:70,137` — challenge LRU is
   `OnceLock<Mutex<LruCache>>` (process-local).** If the agent
   crashes + systemd respawns inside the VM, BOTH the challenge LRU
   and verifier nonce LRU reset. A captured same-sandbox resync
   replayed against the respawned agent passes both gates and re-sets
   `CLOCK_REALTIME`. `sandbox_id` bind still isolates cross-sandbox.
   Mitigation: persist the LRU to a tmpfs file that survives agent
   restart but NOT snapshot/restore. Or bind a monotonic counter into
   the body.

5. **`crates/sandbox-agent/src/handlers.rs:732-741` —
   `boot_sandbox_id()` returns `None` if init was never called;
   handler 500s but the agent still serves traffic.** Today's
   `main.rs:97` exits on init failure so it's unreachable, but a
   refactor that removes the explicit init call would not be caught
   at compile time. Make `boot_sandbox_id` panic on `None` (or move
   the id into `AppState` so the type system requires it).

### MINOR

6. **`crates/sandbox/src/restore_handler.rs:1547,1556` — controller's
   error path embeds 256 chars of the agent response verbatim**
   (`/_clock_resync status {status}: {body_excerpt}`). R5-S4
   sanitised admin sites via `err_safe()` but this restore-internal
   path bypasses it — agent internals leak into controller logs.

7. **`crates/sandbox-agent/src/sig.rs:413` —
   `verify_kind_skew_bypass` still `pub` on `pub mod sig`** (R7-API1
   open). Same anti-pattern R4-S1/R5-API1 closed. A future agent-crate
   caller could accidentally invoke skew-bypass elsewhere.

8. **`crates/sandbox-agent/src/handlers.rs:765` — hex validator allows
   only `b'a'..=b'f'`** (lowercase). Controller side uses lowercase
   `format!("{b:02x}")` (`restore_handler.rs:1591`), so the gate is
   tight today — but a one-line comment "uppercase is rejected" would
   guard against a future controller refactor to `{b:02X}`.

## Standing items not re-verified

F2 (unsigned `/livez` probe), F4 (TBD), R5-S5 (hard_link aliasing
canonical L1) — no commits since r7; remain open per deferred.md.

## Counts

- CRITICAL: 3 (1 new deployment gap; 2 standing — A1, W1)
- MAJOR: 2 (1 LRU-on-restart caveat; 1 type-system hardening)
- MINOR: 3
- Total: 8
