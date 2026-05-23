# Sandbox snapshot-restore — Concurrency Review r6 (2026-05-24)

Branch `feat/sandbox-snapshot-restore` @ `29196e0c`. Read-only.
Worktree `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

Prior rounds: r1-r5. r5 surfaced 2 critical (C3 widened third time;
B17 wrapper subshell un-reaped). This round triangulates bug #22
from code and re-flags un-closed structural debt.

---

## #22 ROOT-CAUSE HYPOTHESIS (NEW CRITICAL)

Appendix E: wake 9/9 200 + `register_restored` fires; first
post-wake `/exec` returns agent 401 on all 9.

Brief hypothesis (a) — agent regenerates keypair on boot — is
REFUTED. `sandbox-agent/src/auth.rs:1-148`: the agent holds ONLY
the controller pubkey, loaded at PID 1 from
`/run/keys/controller-pubkey` (init.sh:54-95 writes it from kernel
cmdline `zsbx_pubkey=<hex>`). Agent never mints/rotates.

Brief hypothesis (b) — wrong sealed bytes — is mechanically blocked
by typed_id round-tripping. `nomad_ch.rs:626-637,867-873`: mint
`sk_bytes`, derive `pubkey`, hex it onto cmdline, seal same
`sk_bytes`. `restore_handler.rs:450-463` +
`nomad_ch.rs:1650-1681` reconstruct
`SigningKey::from_bytes(&sealed.signing_key_bytes)` whose
`verifying_key()` MUST equal the cmdline pubkey the snapshot's
config.json preserves. A sandbox_id mixup would Err at
`nomad_ch.rs:1676` (`Entry::Occupied`).

Most-likely cause: guest wall-clock skew.
`sandbox-agent/src/sig.rs:313-316` rejects when
`|unix_now() - ts| > SKEW_S (5s)`. Agent `unix_now()` at `:565-570`
is raw `SystemTime::now()` = `CLOCK_REALTIME`. A CH `--restore`-ed
guest resumes kvm-clock from the frozen TSC at snapshot time.
`scripts/nomad-vm-wrapper.sh:388-419` issues `ch-remote resume` +
tap-up but NO host-driven clock step (no `clock_settime`, no
chronyc, no VSOCK time bridge). The rootfs has no NTP daemon in the
agent boot path. Guest `CLOCK_REALTIME` lags real wall time by
`(snapshot_age + wake_latency)`. Wake p50=9.7s alone exceeds 5s
skew; real snapshot ages will be far larger.

Ranking:

1. **Guest wall-clock skew (CRITICAL).** Body-hash + key-fp both
   correct; only `ts` stale. Fix shape: post-resume clock-step
   (VSOCK time bridge, ch-remote clock-setter, or init-stage chrony
   makestep).
2. **Nonce LRU survives snapshot (MAJOR).** `sig.rs:228,371` —
   the in-process `Mutex<LruCache>` lives in guest RAM, captured by
   the snapshot. Controller mints fresh nonces post-wake, so
   collision is rare; will surface as the next failure mode once (1)
   closes.
3. **Wrong-canonical-version drift.** `sig.rs:200-202`. Unlikely on
   `/exec` (always V1).

Confirmation step: capture agent `AuthFail::as_str()` in the 401
body (today the wire only says "unauthorized"). `skew-too-large`
confirms (1).

---

## STILL-OPEN (deferred file tracks; re-flagged)

### CRITICAL

- **R5-C1 / C3 (3rd widening).** `restore_handler.rs:450-463` —
  cancel-unsafe window includes `unseal().await` +
  `register_restored().await`. Drop after `wait_for_livez` Ok leaks
  vm_index + leaves pg `Restoring` row + a live VM the state-map
  doesn't know about. Fix: scope-guard covering the restore-Ok
  block.
- **R4-A2 RAII gap.** `backend/nomad_ch.rs:944-952,1087-1091` —
  state-map removal + vm_index release span 60-120s of async fence
  work without unified guard. `register_restored` at `:1650-1681`
  is the 4th state-map insertion path. `LeasedVmSlot` RAII
  subsumes all four.

### MAJOR

- **B17 subshell un-reaped (5th round flag).**
  `scripts/nomad-vm-wrapper.sh:388-419` — `( … ) &` background
  subshell polls + resumes + tap-up logs over ~4.3s; no `wait` and
  no PID-track in `cleanup`. When CH exits at line 449, the
  subshell can outlive parent bash mid-sleep; cleanup trap lacks
  its PID. Three-line fix: capture `RESUME_PID=$!` after line 419;
  add `kill $RESUME_PID 2>/dev/null; wait $RESUME_PID 2>/dev/null`
  to cleanup. Trivial; carrying for 5 rounds is the smell, not the
  bug.

### MINOR

- **R5-P1 BufReader concurrency-neutral** (confirmed). Wake p50
  unchanged (9.7s); dominant cost is un-`spawn_blocking`'d
  `store.get` on ntex worker (R5-P1b).
- **`sig.rs:228` `Mutex<LruCache>` held across `verify_strict`** —
  fine for single-thread agent today; move verify outside the lock
  before any multi-thread variant.

---

## SUMMARY

5 findings: 1 NEW critical (#22 root-cause = guest wall-clock
skew), 2 still-open critical (R5-C1, R4-A2), 1 major (B17 5th
round), 2 minor. Wake-path floor ~62/100 until clock-skew closes —
the signature scheme is sound but the time-bound is unreachable
across a stop-the-world snapshot.

Most-critical citations:
- `sandbox-agent/src/sig.rs:312-316,565-570` — `SkewTooLarge` at
  ±5s with agent `unix_now() = SystemTime::now()`, which on a
  CH-`--restore`-ed guest reflects frozen kvm-clock not host wall.
- `scripts/nomad-vm-wrapper.sh:388-419` — restore branch issues
  `ch-remote resume` but never step-syncs the guest clock. The
  missing post-resume clock step makes the entire signed-RPC
  surface unreachable post-wake.
