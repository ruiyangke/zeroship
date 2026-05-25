# Proposals triage — auto-managed by /loop

Each loop cycle: pick 1-2 NOT-yet-done proposals, determine status against
current code + decisions/ + git history, then act:
- **SHIPPED w/ ADR** → the ADR + reference docs already capture it; move proposal to `docs/archive/` (git mv) with a one-line header noting where the live docs are.
- **SHIPPED w/o ADR** → if it holds reference-grade content not in `docs/reference/`, promote that content into the right reference doc (or a new one); then archive the proposal. If fully covered, just archive.
- **ABANDONED / superseded** → archive with a note (pre-launch: deletion is fine, but archive keeps the design rationale).
- **STILL ACTIVE (unshipped)** → leave in place; mark `[ACTIVE]` here.

Verify every claim against code before promoting. Reference docs must be accurate + helpful, repo-relative links, concise.

## Queue

### SHIPPED — has ADR in decisions/ (archive; content already in ADR + reference)
- [ ] compression-streams-native.md → 2026-05-01-compression-streams-native ADR
- [ ] headers-native.md → 2026-05-01-headers-native ADR
- [ ] fetch-native.md → 2026-05-02-fetch-native ADR
- [ ] streams-native.md → 2026-05-02-streams-native ADR
- [ ] webcrypto-native.md → 2026-05-02-webcrypto-native ADR
- [ ] websocket-native.md → 2026-05-02-websocket-native ADR (+ reference/websocket-design.md)
- [ ] macro-constructor-post-init.md → 2026-05-04 ADR
- [ ] macro-v8-state.md → 2026-05-04 ADR
- [ ] node-crypto-native.md → 2026-05-05-node-crypto-native ADR

### SHIPPED — merged to main, no ADR (promote any reference-grade content, then archive)
- [ ] nomad-driver-ch.md (merged dde08ca2; ops covered by runbooks/sandbox-nomad-ch.md)
- [ ] sandbox-snapshot-restore.md (merged d1054adb)
- [ ] sqlite-pg-parity.md (merged; reference/sqlite-divergences.md exists)
- [ ] sandbox-pg-state.md (feat/sandbox-pg merged)
- [ ] sandbox-preview-urls.md (sandbox preview merged)
- [ ] kv-redesign-implementation-plan-2026-05-24.md (kv redesign; reference/kv.md exists)
- [ ] zeroship-db.md (db SDK; reference/db.md exists)
- [ ] db-system-design.md (db; reference/db.md)
- [ ] platform-system-fields.md (db system fields)
- [ ] sensitive-field-masking.md (db masking)
- [ ] p0-implementation-plan.md (DB P0)
- [ ] p1-sqlite-implementation-plan.md (DB P1)
- [ ] p4-search-implementation-plan.md (DB P4)
- [ ] p5-encryption-backup-implementation-plan.md (DB P5)
- [ ] runtime-macros-refactor.md (runtime-macros; reference/plugin-system.md)
- [ ] zs-standard-and-vite-v2.md (reference/zs-standard.md + vite-plugin.md exist)

### NEEDS DETERMINATION (check shipped vs active)
- [ ] rpc.md
- [ ] plugins-workers-distributed.md

### KEEP — living docs, not a ship-once proposal
- [x] feature-roadmap.md  [ACTIVE — keep in place]

## Done log
(loop appends: PROPOSAL — action — commit)
