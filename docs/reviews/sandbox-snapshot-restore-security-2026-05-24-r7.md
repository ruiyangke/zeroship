# Security review — sandbox-snapshot-restore (r7)

HEAD `6f5d41b8`. Focus: B22 `/_clock_resync` + re-audit
A1/W1/R5-S5/F4.

B22 verdict: skew-bypass replay-safe within one VM lifetime, but
has one inter-restore replay window + one fail-open default.
Ed25519 trust anchor intact (private key required). Severity: high.

## R7-SEC-1 — CRITICAL: cross-restore replay of `/_clock_resync`

`sandbox-agent/src/sig.rs:356-449`, `restore_handler.rs:1423-1484`.

Body is `{"ts": <unix_secs>}` — no sandbox-id, no
restore-session-id, no counter. Nonce LRU is the *only* replay
defense (skew gate explicitly bypassed, `sig.rs:389`).

LRU lives in agent heap (`sig.rs:228 Mutex<LruCache>`). CH
`--snapshot` captures heap. The doc at `sig.rs:348` claims "the LRU
survives indefinitely across restores" — true only for nonces seen
*before* the snapshot. The resync arrives *after* restore, so no
resync nonce is ever in any snapshot's LRU.

Attack: capture cycle-N resync wire bytes (network-adjacent on the
controller↔VM L2 path); on cycle N+1 race the controller's POST.
Signature passes (same signing key, same canonical), skew gate
bypassed, `settimeofday(2)` writes the stale `T_old`. If attacker
wins or sustains the race, guest clock holds at `T_old` and every
strict-skew RPC 401s → sustained DoS. Fix: bind `sandbox_id` + a
per-restore controller challenge into the canonical body.

## R7-SEC-2 — MAJOR: `derive_agent_url` default returns `127.0.0.1:0`

`sandbox/src/restore_handler.rs:182-184`. Any backend that forgets
to override silently dispatches the controller-signed
`/_clock_resync` to localhost. Port 0 usually fails, but a
co-tenant listening locally would capture a valid signed wire.
Fail-open default; should `panic!` or return `Err`.

## R7-SEC-3 — MAJOR (still open): A1 AEAD never wraps prod store

`sandbox/src/lib.rs:612-631` plumbs
`LocalDiskSnapshotStore`/`TieredSnapshotStore<_,GcsSnapshotStore>`
as `Arc<dyn SnapshotStore>` directly. `AeadSnapshotStore::new(...)`
never called; `RootKek::from_env` at `snapshot_aead.rs:208` has no
caller. GCS artifacts unencrypted at app layer — bucket ACL is the
sole defense.

## R7-SEC-4 — MAJOR: `/_clock_resync` admits arbitrary stale `ts`

`handlers.rs:579-636`. Body `ts: u64` accepted as-is and passed to
`settimeofday(2)`; agent has no upper bound. Combined with
R7-SEC-1, a replayed cycle-N resync sets the clock backwards by
days. Defense-in-depth: reject `ts < state.started_at_unix - N` or
`|ts - now_monotonic_estimate| > 24h`.

## R7-SEC-5 — MAJOR: F4 — `signing_key_bytes` still a `pub` field

`sandbox/src/persist.rs:140` declares `pub signing_key_bytes:
[u8; 32]`. Used at `restore_handler.rs:492,503` as bare field
access on raw secret material. No `Zeroizing`/`Secret` wrapper, no
Debug redaction at the destructuring site.

## R7-SEC-6 — MAJOR (still open): R5-S5 hard_link inode aliasing

`sandbox/src/snapshot_store.rs:266`. `get()` hard-links L1 cache
into the caller's destination. A concurrent `put()` on the same
content-addressed sha256 shares the inode; write through one
handle racing CH `--restore`'s read corrupts the restore. AEAD
per-chunk auth would catch it, but A1 is unwired, so silent.

## R7-SEC-7 — MINOR: W1 wrapper `sed` rewrites untrusted JSON

`sandbox/scripts/nomad-vm-wrapper.sh:367`. `sed -i -E` on
`$ZSBX_RESTORE_FROM/config.json` whose content came from CH
`--snapshot` of a tenant VM. Unbounded substitution — alloc paths
embedded in JSON escapes can corrupt the document. Replace with a
Rust-side parse+rewrite.

## R7-SEC-8 — MINOR: idempotency doc-comment overstated

`handlers.rs:566` claims "a re-call with a fresh ts/nonce simply
re-sets the clock" — fresh-nonce path is fine, but same-wire replay
only hits `ReplayedNonce` if the LRU still holds it. Under
R7-SEC-1 the LRU starts empty for resync nonces at each restore.
Doc needs "within one VM lifetime" caveat.

---

## Summary

**8 findings**: 1 CRITICAL, 5 MAJOR, 2 MINOR. Most critical
citations: `sandbox-agent/src/sig.rs:348-355` (LRU survives restore
— true only pre-snapshot) and `sandbox/src/restore_handler.rs:182-184`
(localhost fail-open default). **B22 audit verdict: REPLAY RISK.**
Nonce LRU does not protect across snapshot cycles because resync
nonces are generated after restore and were never in any snapshot's
LRU. In-VM forgery still impossible (private key required), but a
network-adjacent attacker can replay a captured resync across
restore cycles to push the guest clock backwards → sustained 401
DoS. Bind `sandbox_id` + a per-restore controller challenge into
the canonical body before production.
