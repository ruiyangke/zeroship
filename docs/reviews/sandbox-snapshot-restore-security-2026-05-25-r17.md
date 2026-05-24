# Sandbox/snapshot-restore — security r17 review

Date: 2026-05-25 (UTC)
HEAD at audit: `370d13e6` (catchup, 2 rounds past r16's `a0888d9e`).
Scope: 5× R16-S* gate verification, GATE-C2 + smoke-r12 KEK
provisioning + C-7-LT-1 async budget surface review. Read-only.

## Summary

All five r16 gates landed in `4ab58eac`+`b2b6c3c9`+`fa4fe63c`.
Four CLOSED, one **PARTIAL** (R16-S2: sanitizer covers RFC1918 +
fe80::/10 + http(s) agent URLs, **misses 169.254/16 IPv4 link-local
and 100.64/10 CGNAT** — the runtime's own SSRF guard
`crates/runtime/src/transport/ssrf.rs:45-51` treats both as private,
so the wake-error sanitizer ships a strictly weaker policy than the
sibling-crate baseline; the gap is the same R18-M1 concurrency
flagged). 2 new findings (R17-S1 sanitizer scope gap promoted to
IMPORTANT-now because the column is SELECT-able by `sandbox_app`;
R17-S2 KEK file owner is implicit-root via systemd unit lacking
`User=` — works, but the contract is undocumented and brittle).

GATE-C2 (`db248cbf`+`678ec197`+`1cfc9182`) and C-7-LT-1
(`f9996fcf`) introduce **no new security gaps**: the admin API is
single-trust-level (admin-token gated; no per-tenant authz at the
wake layer), and the 70 s async budget is contained inside a
detach_isolated thread that does not bind a client deadline. The
DoS concern is theoretical at admin-only call-sites.

## R16 gate verdicts

- **R16-S1 — audit-role SELECT revert: CLOSED** at
  `fa4fe63c::0010_wake_jobs_hardening.sql:33-41`. `REVOKE SELECT ON
  sandbox.wake_jobs FROM sandbox_audit` wrapped in
  `DO $$ … pg_roles WHERE rolname='sandbox_audit' … END $$` (safe on
  dev pg). Aligns with `0004_role_split_phase3.sql:62-70`'s
  INSERT-only invariant.
- **R16-S2 — sanitize error_message: PARTIAL** at
  `b2b6c3c9::wake_machine.rs:717`. RFC1918 (10/8, 172.16/12,
  192.168/16) ✓; fe80::/10 ✓; http(s) agent URLs over RFC1918 ✓;
  256-byte char-boundary-safe truncation ✓. **Missing**: 169.254/16
  (IPv4 link-local, AWS/GCP IMDS), 100.64/10 (CGNAT — AWS VPC ENIs,
  some k8s clusters). See R17-S1 below.
- **R16-S3 — agent_url CHECK constraint: CLOSED** at
  `fa4fe63c::0010_wake_jobs_hardening.sql:56-68`.
  `CHECK (agent_url IS NULL OR agent_url ~
  '^https?://[a-zA-Z0-9._:/-]+$')` wrapped in
  `information_schema.constraint_column_usage` existence guard
  (idempotent). Tight punctuation set — no spaces, no query string,
  no fragment — prevents arbitrary-text injection from a
  misbehaving controller.
- **R16-S4 — WakeResponseMode fail-CLOSED: CLOSED** at
  `4ab58eac::config.rs:893-902`. `Ok("async") → Async`,
  `Ok("sync"|"") | Err(_) → Sync`, **`Ok(other) → Err(format!(...))`**.
  `pub fn from_env() -> Result<Self, String>` propagates through
  `SandboxConfig::from_env`. Mirrors R15-S1 AEAD pattern.
- **R16-S5 — gc retention configurable: CLOSED** at
  `4ab58eac::config.rs:921-970`. New `WakeLifecycleConfig` struct:
  default `300 s`, env `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`,
  invariant `n >= MIN_GC_RETENTION_SECS (1 s)`. Zero / unparseable
  values return `Err` — boot refuses. Justification comment notes
  "must exceed the client's max polling interval — otherwise a
  client that observes a row terminal-ok at T and re-polls at T +
  retention sees a 404 wake_not_found". Sound.

## Findings (NEW since r16)

### [R17-S1] `sanitize_error_message` omits 169.254/16 + 100.64/10 — promoted IMPORTANT (security-r17)

- **Files**: `crates/sandbox/src/wake_machine.rs:706-723` (the
  pass list) + `:748-779` (`match_rfc1918_at` prefix table).
- **Symptom**: the sanitizer's prefix table covers `10.`, `172.`
  (16-31), `192.168.` — but **not** `169.254.` (RFC 3927 IPv4
  link-local, used by AWS/GCP/Azure IMDS at `169.254.169.254`) and
  **not** `100.64.` (RFC 6598 CGNAT, used by AWS VPC ENIs and some
  managed k8s control planes). Both are cluster-topology by the
  platform's own definition: `crates/runtime/src/transport/ssrf.rs:45-51`
  treats them as private (`is_link_local()` + the explicit
  `octets[0]==100 && (octets[1] & 0xC0)==64` test). The sibling
  k8s NetworkPolicy at `crates/sandbox-agent/k8s/networkpolicy.yaml:90-91`
  blocks `169.254.0.0/16` and `100.64.0.0/10` from sandbox egress.
- **Threat model**: PR2 wiring per `wake_machine.rs:153` pipes
  failure messages from `restore_handler` and `backend.*` into
  `update_wake_job_state(_,_,_,Some(sanitized),_)`. On AWS/GCP a
  failed IMDS or VPC-internal probe surfaces e.g.
  `"connect 169.254.169.254:80 timed out"` or
  `"agent /livez 503 from 100.64.5.42:7000"`. With R16-S1 closed
  the column is SELECT-able by `sandbox_app`. Worker-internal IPs +
  IMDS endpoints leak through a column reachable by app-role
  reads.
- **Why IMPORTANT-not-MINOR**: the runtime crate ALREADY treats
  these prefixes as private at egress (`ssrf.rs`). Shipping a
  weaker policy on the inbound-error sanitizer is a doctrine drift
  inside the same workspace. The sibling NetworkPolicy proves the
  ops team considers these prefixes cluster-topology. The fix is a
  4-line addition to the prefix table.
- **Action**: extend `match_rfc1918_at` to also match
  - `169.254.\d{1,3}.\d{1,3}` (4 bytes consumed, 2 octets remain)
  - `100.\d{1,3}.\d{1,3}.\d{1,3}` constrained to second-octet
    `64..=127` (parse like the `172.` arm)
  add proptest pins for `169.254.169.254` and `100.64.0.1`,
  rename helper from `strip_rfc1918` to `strip_private_ipv4` to
  reflect the broader scope. Also update doc comment line 706-708
  to enumerate the new prefixes.

### [R17-S2] Snapshot root KEK owner is implicit-root via missing `User=` directive (MINOR posture, security-r17)

- **Files**: `crates/sandbox/scripts/gcp-worker-startup.sh:431-439`
  (KEK gen) + `:456-549` (`zsbx-ctl.service` unit body).
- **Symptom**: `head -c 32 /dev/urandom > "$ROOT_KEK_PATH"` runs
  inside the startup script's outer context (root). Mode `0400`
  ✓; size 32 bytes ✓; idempotent (`[ ! -s "$ROOT_KEK_PATH" ]`) ✓;
  source `/dev/urandom` ✓. **Owner is set implicitly** to the
  process EUID (root, since the startup script is invoked from
  gcp instance-startup) — there is no explicit
  `chown root:root "$ROOT_KEK_PATH"`, and the `zsbx-ctl.service`
  unit body (lines 462-549) defines no `User=` / `Group=` so
  systemd falls back to root. Today the chain works; the
  brittleness is that any future move to a non-root service user
  (defense-in-depth) silently breaks read (KEK is `0400 root:root`,
  controller as e.g. `zsbx`-user would EACCES on `from_path`).
- **Threat model**: not a present leak — `0400 root:root` +
  `User=root` is exactly what's documented in `crates/sandbox/src/lib.rs:1005-1028`
  for `assert_kek_required_for_remote_store`. The risk is that a
  future operator hardening the service unit ("least privilege —
  drop to a zsbx user") would land a config that boot-fails with
  an EACCES that points at the KEK file rather than the unit
  change. The fail-CLOSED boot assertion converts that into a
  "snapshot disabled" startup error, not a leak.
- **Why MINOR**: today's posture is correct. Documentation gap.
- **Action**: (a) add `chown root:root "$ROOT_KEK_PATH"` after
  generation to make ownership explicit (defensive — runs as a
  no-op today); (b) add a comment in the unit body line 523-525
  *"KEK is `0400 root:root`. If you ever add `User=` here, you
  must also `setfacl -m u:<svc>:r "$ROOT_KEK_PATH"` or move the
  KEK to a group readable by both. The boot assert in
  `assert_kek_required_for_remote_store` will fail-CLOSED if the
  controller cannot read the file."* Doc-only beyond the explicit
  chown.

## Hunt disposition (terse)

1. **GATE-C2 cross-tenant probe?** No. `POST /admin/sandboxes/{id}/wake`
   gates on `state.admin_token` (`admin_handlers.rs:126-149`). There
   is no per-tenant authz at the wake/poll layer — the admin caller
   is trusted to specify any sandbox_id. A malicious admin caller
   already has full sandbox CRUD. No new probe surface from C2.
2. **C-7-LT-1 70 s async budget DoS?** Budget runs inside
   `detach_isolated` on a dedicated OS thread per `wake_machine.rs`
   spawn site; no ntex client deadline. Slot pressure is bounded by
   the `vm_index_floor..vm_index_ceil` range and the
   `wake_jobs_sandbox_pending_uniq` partial UNIQUE INDEX
   (`0011_wake_jobs_unique.sql`) limits each sandbox to one pending
   row. Mass-POST → ON CONFLICT → Replay → no second machine spawn.
   No new DoS path beyond what an admin caller already has.
3. **R13-S1 (carry, IMPORTANT)** — `provision-gcp-cluster.sh:286`
   still `--scopes=storage-rw` with no `--service-account`.
   UNCHANGED.
4. **R9-S3 (carry, IMPORTANT)** — `lib.rs:1008,1057` +
   `db.rs:2530-2542` stamp `snapshot_aead_dek_id="v1"`
   unconditionally. UNCHANGED.
5. **R15-S3 (carry, MINOR)** — `gcp-worker-startup.sh:488` host_fence
   cap=30s comment still has no security-rationale sentence.
   UNCHANGED.
6. **0010 + 0011 migrations** — both idempotent (DO $$ existence
   guards + `IF NOT EXISTS`), forward-only. No rollback DDL needed.
7. **Sanitizer test coverage** — `wake_machine.rs:1001-1110` covers
   10/8, 172.16/12 (incl. 172.15/172.32 negative pins), 192.168/16,
   ports, https URLs, IPv6-LL, 256-byte truncation at char
   boundary, multi-IP messages. **No pin for 169.254/100.64** —
   confirms R17-S1.

## Carry-forward open at HEAD `370d13e6`

R13-S1 (IMPORTANT) · R12-S1 partial (IMPORTANT) ·
R10-S1/S2 (IMPORTANT) · R9-S2/S3 (IMPORTANT) · R14-S2 ·
R13-S2 · R11-S3 · R10-S3 · R9-S6/S7/S8 · R15-S3 (all MINOR).
R9-S1 partially closed by R15-S2.

Closed since r16: R16-S1 (`fa4fe63c`), R16-S3 (`fa4fe63c`),
R16-S4 (`4ab58eac`), R16-S5 (`4ab58eac`). R16-S2 PARTIAL — gap
re-filed as R17-S1.

## Counts

- CRITICAL: 0 new; carry: R9-S1 partial.
- IMPORTANT: 1 new (R17-S1); carry: R13-S1, R12-S1 partial, R10-S1,
  R10-S2, R9-S2, R9-S3.
- MINOR: 1 new (R17-S2); carry: R14-S2, R13-S2, R11-S3, R10-S3,
  R9-S6/S7/S8, R15-S3.
- Total NEW this round: 2.

## Cross-lens consensus

- **concurrency-r18 (already filed M1)**: R17-S1 is the same gap.
  Promoted from MINOR to IMPORTANT here because the column is now
  SELECT-able by `sandbox_app` (R16-S1's CLOSED state changes the
  threat model — pre-r17 audit-role-only meant low blast radius;
  post-revoke `sandbox_app` is the read surface, and that role is
  the app-runtime tenant).
- **arch-r18**: GATE-C2's INSERT-then-read-on-conflict shape is the
  right idempotency primitive. No security objection.
- **test-cov**: R17-S1 closes via two new pins in the existing
  `sanitize_*` test module.
- **api-surface**: agent_url CHECK regex `[a-zA-Z0-9._:/-]` allows
  hyphenated hostnames but rejects DNS labels with `~` or `?`; this
  is correct for the agent's `derive_agent_url()` shape but worth
  documenting if external callers ever land.

## Lens hand-off

- **Architecture**: R17-S2 (a) explicit `chown root:root` in
  `gcp-worker-startup.sh` after KEK gen.
- **Concurrency**: R17-S1 closure — extend `match_rfc1918_at` (or
  rename to `match_private_ipv4_at`) with 169.254/100.64 arms.
  Same module as the existing sanitizer.
- **Test-cov**: R17-S1 proptest pins for `169.254.169.254`,
  `100.64.0.1`, `100.127.255.254` (CGNAT upper edge); negative
  pins for `100.63.x.y` (below CGNAT range) + `100.128.x.y` (above
  CGNAT range).
- **Doc**: R17-S2 (b) systemd unit body comment + R15-S3 fence
  rationale carry. Both doc-only.
