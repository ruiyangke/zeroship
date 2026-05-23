# Test-coverage review — 2026-05-24 round 3

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: 4340e3b5
**Lens**: test-coverage
**Last reviewed (this lens)**: 2026-05-24 r2

## Summary

7 findings (2 CRITICAL, 3 IMPORTANT, 2 MINOR). r2 #2 PINNED by A2
helper test. r2 #1 (A1) and r2 #3 (T6) both **open** — deferred A1/T8.
T7 wall-time bound is CI-flaky-prone; A6 tests cover happy path only.
`cargo test -p zeroship-sandbox --lib` = 230 passed, 0 failed, 1 ignored.

## CRITICAL

### 1. r2 #3 still open — T6 `ControllerIdleSnapshotter` zero non-pg coverage (deferred T8)
`crates/sandbox/src/sweep.rs:296-387`. T7 added concurrency tests but
the *production* `IdleSnapshotter` impl is untouched: wiring-trio
guard `sweep.rs:317-324`, `StateMismatch → debug` swallow at 372-381,
teardown-failure warn at 357-369, `lookup_source_vm_ops` error map
at 329-332 — **all uncovered**. A regression inverting the Ok/Err
arms of the lookup match still passes every test. **Fix**: extract
a pure `classify_snapshot_outcome` helper and table-drive it; the
guard at 317-324 is testable via `AppState::new_fixture` (A5) plus
a mock backend.

### 2. r2 #5 still open — wrapper has zero in-repo coverage; B17 cannot regression-trip locally
`crates/sandbox/scripts/nomad-vm-wrapper.sh` (421 LOC) + `init.sh`
(141 LOC) — no `bash -n`, `shellcheck`, or bats. B17 (deferred 22-34)
bottoms out at the `restore` branch lines 308-384: the `sed -i -E`
rewrite at 359-363 (W1 RCE still open, deferred line 92) and the
3-iter tap-up retry at 378-384. Branch divergence between `start`
(lines 393-401, full `--kernel/--cmdline/--disk/--net`) and `restore`
(lines 366-369, bare `--restore source_url=…`) is where B17 hides.
Drop-in still uncreated: `tests/wrapper_lint.rs` with `bash -n` +
`shellcheck --severity=error` catches regressions for free. A `bats`
smoke mocking `cloud-hypervisor` + `ip` could pin the tap-up retry
loop without a real cluster.

## IMPORTANT

### 3. T7 wall-time assertion is CI-flaky-prone
`sweep.rs:663-700`. Sleeps 100ms/row, caps concurrency at 4 over 8
rows, asserts `elapsed < 500ms` (line 694) — 2.5× over 200ms ideal
but only 1.6× under the 800ms serial floor. On loaded CI with compio
scheduling jitter this transient-fails without a real regression.
The `max_in_flight >= cap` gate (684-688) is the robust assertion —
proves overlap without wall-time. **Fix**: keep `max_in_flight`,
demote elapsed to `eprintln!` or raise to 700ms (still <800ms floor,
3.5× slack). This will be the first test disabled when CI flakes.

### 4. A5/A6 builders cover happy/empty only — no invalid-input fuzz
`lib.rs:1292-1337` (A5) + `lib.rs:1439-1483` (A6). A5 covers `None`,
`Some("")`, `Some("operator-bearer-abcdef")`. **Missing**: 1-byte
token, all-whitespace `Some("   ")` (current impl ACCEPTS — then
`admin_check` permits `Bearer    `), NUL-embedded, 4 KiB token. The
doc at `lib.rs:184-187` only specifies empty rejection; everything
else is silently accepted. Either tighten (whitespace-trim + min
length) or pin the permissive policy explicitly. A6 covers
`accepts_arc` + `replaces_existing` only — no test pins `persist()`
accessor visibility (`lib.rs:266`); a regression to `pub` lights
no red.

### 5. r2 #4 still open — `read_snapshot_row` decode tail has no non-pg test
`crates/sandbox/src/restore_handler.rs:242-280`. r1 → r2 → r3
unchanged: no `decode_snapshot_row` extraction, no unit cases for
missing-vm_index / wrong-sha-length / well-formed. The pg-gated suite
(60 `#[ignore]`d in `tests/sandbox_pg_e2e.rs`) covers happy path only.

## MINOR

### 6. A2's `verify_canonical_sha256_from_streams` corner cases under-tested
`snapshot_store_gcs.rs:621-664`; test at 1176-1241 covers honest pass,
tampered bytes, declared length > served. **Missing**: (a) zero-byte
file — does `loop { read } if n==0 break` exit cleanly? (b)
exactly-chunk-boundary file (`len = 65536` matches the `vec![0u8;
64*1024]` buf at line 633), (c) `checked_add` overflow at 641-643
(needs fake reader returning `usize::MAX`), (d) extra-trailing-bytes
(body longer than declared len — does line 645-651 fire?). All four
are 5-line additions on the existing `Cursor` fixture.

### 7. No property-based testing where the math matters
`proptest`/`quickcheck` absent from the crate. Three sites: (i) AEAD
chunk encoder `snapshot_aead.rs:363-450` — `div_ceil(CHUNK_PLAINTEXT_LEN)`
at line 383 and chunked loop at 404-440; a `proptest` over `total_len
∈ [0, 10*CHUNK_PLAINTEXT_LEN]` shrinks to zero-byte, off-by-one-around-
1MiB, 4 GiB-edge cases the single 5MiB+12345B test at line 731 cannot.
(ii) `verify_canonical_sha256_from_streams` (finding 6) — proptest
over `Vec<(name, body)>` auto-shrinks to the four corner cases. (iii)
typed_id base62 codec `crates/core/src/typed_id.rs:41` — 5-line
round-trip `proptest` over `Uuid` pins the codec better than any
hand-written fixture.

---

## What's pinned vs. open from r2

| r2 finding | Status |
| --- | --- |
| #1 A1 (AEAD not wired in `from_config`) | **Open** — deferred A1 unchanged. |
| #2 A2 (GCS `verify` discards sha) | **Pinned** by `snapshot_store_gcs.rs:1176-1241`. Corner gaps (#6). |
| #3 T6 (`ControllerIdleSnapshotter` no non-pg cov) | **Open** (this round #1; deferred T8). |
| #4 r1-C2 (`read_snapshot_row` decode tail) | **Open** (this round #5). |
| #5 r1-C3 (wrapper zero in-repo cov) | **Open** (this round #2). |
| #6 r1-I2 (sweep async no-op no non-pg) | **Open**, not re-checked. |
| #7 r1-I2b (`RecordingIdleSnapshotter` fail-knob unused) | **Open**, not re-checked. |
| #8 wake e2e local alternative | **Open**, not re-checked. |
| #9 r1-M1 wrapper/controller MAC+TAP drift | **Open**, not re-checked. |
