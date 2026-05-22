---
name: pilot-cron-worker
description: Use when the user grants autonomous pilot authority and asks for self-driving improvement of a target codebase via a recurring cron job. Encodes the two-part cycle (deep review + backlog drain), verification discipline, failure handling, and scope guards.
---

# Pilot Mode + Cron Worker

## When to use

Activate when the user says any of:
- "you are the pilot now"
- "never stop working, never ask for my approval"
- "run [reviews/improvements] every N minutes/hours"
- "drain the backlog automatically"
- "set up a cron to keep working on X"

The mandate is: **continuous improvement of a specific codebase area, without user prompts between cycles.**

## The two-part cycle

Each cron fire MUST execute both halves. Never skip Part 2 just because Part 1 found work.

### Part 1 — Deep review (find new findings)

Dispatch **3-5 sub-agents IN PARALLEL**, each with a different lens, all read-only. Rotate lenses across cycles so coverage stays uniform.

Standard lenses (pick 3-5 per cycle, rotate):
- **Architecture** — module boundaries, layering, coupling, extension points
- **Code quality** — language-idiomatic patterns, error handling, panic risks, lifetime/ownership
- **Concurrency** — race conditions, lifecycle, lock discipline, async/await traps
- **Performance** — hot paths, allocation cost, FFI overhead
- **Security** — injection surfaces, identifier quoting, credential handling, isolation
- **Test coverage** — unit-test gaps, integration stability, missing edge cases
- **API surface** — `pub` leaks, `#[doc(hidden)]` exposures, error envelope consistency

Each agent writes its report to `docs/reviews/<target>-<lens>-YYYY-MM-DD-r<round>.md`. The fixer picks up the round number by inspecting existing files.

### Part 2 — Backlog drain (pick up deferred items)

Read EVERY existing review under `docs/reviews/<target>-*.md` AND `docs/reviews/<target>-deferred.md`. Compile the union of all open IMPORTANT and CRITICAL findings still tagged deferred / still-pending / out-of-scope. For each:

1. **Re-evaluate actionability** — has the blocker cleared? (API was overloaded, prior refactor done, test infra fixed)
2. **Re-evaluate scope** — is the finding still relevant after recent commits?
3. **Pick the 1-2 highest-leverage items** that are now actionable AND not already in flight this cycle
4. Dispatch focused fixers with the finding's file:line + current repo state

The deferred file IS the backlog. When an item is fixed and verified, remove it from the file (commit the cleanup). When a new opinion-bound or blocked finding emerges, append it with file:line + blocker reasoning. **Never silently drop findings.**

## Verification discipline

Trust-but-verify every sub-agent report.

For each returning agent:
1. **Verify what landed** — read the actual diff, run tests, check build. Agent reports describe intent, not necessarily reality.
2. **Judge quality** — does work match the brief? Silent scope reductions? Skipped TDD? Weakened decisions?
3. **Decide**: accept / retry with tightened prompt / split into smaller chunks.
4. **Report findings with evidence** — commit hashes, line counts, test counts, smoke output lines — not just the agent's self-report.

**Concrete failure modes to watch for:**
- Agent claims "all smokes passed" but didn't actually re-run them after a late-stage refactor → require evidence of the actual line.
- Agent encounters a failure introduced by the prior agent and mis-attributes it as "pre-existing" → verify by running the same gate at the prior commit (`git stash + git checkout HEAD~ + run gate`).
- A typed-cast refactor silently drops a `this` binding for v8_class methods → grep after any refactor that touches `native.X` call sites.
- Manual fix misses SIBLING instances in adjacent files → grep the codebase for the same pattern before declaring done.
- Agent over-claims: 140 tool uses + API 529 on final write may mean the work landed in commits but the report didn't. Inspect `git log` to confirm what actually landed.

## Failure handling

### API 529 (overloaded)

Retry rules:
1. **First retry**: dispatch the same task with `model: "sonnet"` instead of `model: "opus"`. Sonnet is sufficient for mechanical fixes and well-scoped diagnoses.
2. **If sonnet also 529**: skip THAT lens for this cycle — but NOT the cycle. Document the API outage in the round's report. The skipped lens rotates back in next cycle.
3. **Don't restart the whole cycle** because one agent failed.

### Test hangs / flakes

If a test suite is known-flaky (e.g., integration tests with intermittent timing issues):
1. Run the deterministic gates (`--lib`, smoke) as primary.
2. Run the flaky gate best-effort with a generous timeout.
3. **Don't let the flake block the cycle** — document and move on.
4. The full fix for the flake is a backlog item, not a blocker.

### Agent conflict on file scope

If multiple agents touch the same file simultaneously, merge conflicts can happen. Strategy (in priority order):

1. **`isolation: "worktree"` for any non-trivial multi-file fixer.** This is now the default for complex work. Each agent gets its own checkout; commits land in the worktree's branch; pilot fast-forwards / cherry-picks back. Eliminates the `git stash` interleaving cascade we observed (5+ stashes accumulating when 5 agents ran concurrently on plugin-db; one agent stashing the prior's WIP just to do its own edits; test builds breaking transiently mid-coordination).

   Use worktree isolation when the fixer:
   - Touches 2+ files, OR
   - Is non-trivial (refactor, type sweep, pipeline split), OR
   - Will run for >5 minutes, OR
   - Depends on a known-shared file (`audit.rs`, `register_model/*`, etc.)

   Skip worktree isolation only for:
   - Single-file single-line mechanical edits
   - Read-only reviewers (no writes)
   - Test additions in isolated test files

   Pattern:
   ```
   Agent({
     description: "...",
     subagent_type: "general-purpose",
     model: "opus",
     isolation: "worktree",       // ← the key flag
     run_in_background: true,
     prompt: "...",
   })
   ```

   On return: if changes landed, the result names the worktree path + branch. Pilot then fast-forwards or cherry-picks. If no changes, the tool auto-cleans the worktree directory and branch.

2. **Dispatch in waves** when worktree isn't appropriate. Wait for the first wave to land, then dispatch the second.

3. **Group by file** — if 3 findings all touch `apply.rs`, give them to ONE agent.

4. **Stagger by 30s** only as a last resort.

## Push authority

When the user grants pilot mode, also confirm push authority. Common phrasings:
- "you have full pilot authority" → implies push
- "never ask for approval" → implies push
- "do them all" → ambiguous; depends on prior session context

**Push after each green verification.** Don't accumulate large unpushed delta — small frequent pushes are easier to debug if something breaks remotely.

CLAUDE.md's default "never push without confirmation" is OVERRIDDEN by explicit user grants. Track the grant in your reasoning so subsequent ticks don't re-ask.

**Still refuse:**
- Force-push to main/master without an explicit additional grant
- `--no-verify` on hooks
- Amend / rewrite already-pushed commits
- Push to repositories you weren't granted authority over (different remote, different branch)

## Scope guards

The cron's prompt should explicitly name:
- **Target directory** — e.g., `crates/plugin-db/`
- **Allowed sibling deps** — e.g., the `@zeroship/bootstrap` package's `installSchema` path
- **Forbidden zones** — public deploy contracts, unrelated crates (`gateway`, `control`, `worker`)
- **Cross-crate change rule** — only when a finding genuinely demands it; otherwise document and defer

A drift into the forbidden zone is a `git restore` + re-dispatch case, not a "let me just fix this too" case.

## Cron job shape

Use `CronCreate` with:
- **Off-minute fire times** (avoid `:00` and `:30` — the global fleet collides there). Prefer `:13/:43`, `:17/:47`, `:07/:37`.
- **Recurring** (`recurring: true`).
- **Session-only** unless the user explicitly says "survive restarts" (`durable: true`).
- **7-day auto-expire** — flag this to the user; they may want to re-arm or move to a CI cron instead.

The cron's `prompt` field MUST be a complete self-contained instruction. The fired prompt has no session memory — it has to re-establish:
- Target codebase
- Both Part 1 and Part 2 mandates
- Verification rules
- Failure handling
- Scope guards
- Push authority reaffirmation

## Anti-patterns

- **Don't ask for approval at the end of a cycle.** Pilot mandate is permanent until revoked.
- **Don't summarize work the user can see in `git log`.** Provide evidence (hashes, test counts, smoke lines), not narration.
- **Don't dispatch a single agent and call it a "review cycle."** Parallel multi-lens is the point.
- **Don't skip Part 2 because Part 1 was productive.** Backlog drain is half the value.
- **Don't push before verifying.** Smoke + lib tests + relevant integration suite are the minimum gate.
- **Don't silently drop a finding.** If it's not actionable, write it to the deferred file with the blocker reason.
- **Don't let one stuck process consume an entire cycle.** Test for hang (CPU% on the process); kill after a reasonable timeout (5-10 min for integration suites); document the hang.

## Backlog file format

`docs/reviews/<target>-deferred.md`:

```markdown
# <target> — Deferred Backlog

Auto-managed by the pilot-cron-worker. Last reviewed: YYYY-MM-DD HH:MM.

## CRITICAL (blocked or needs design)

### [I1] Backend trait half-closed
- **Source**: architecture round-2 critic, 2026-05-21
- **File**: crates/plugin-db/src/backend/mod.rs:68-352
- **Blocker**: needs design decision — shrink trait or grow it. Awaits user input.
- **Last considered**: 2026-05-22 17:47 — still blocked

## IMPORTANT (mechanical, lower priority)

### [I3] 7 mint_* copy-paste pattern
- **Source**: code-critic, 2026-05-21
- **File**: multiple v8_classes/*.rs
- **Blocker**: needs a typed MintGuard abstraction designed
- **Last considered**: 2026-05-22 17:47 — could pick up next cycle

## MINOR

### [Q2] cargo-clippy + cargo-audit not installed
- **Source**: quality-evaluator, 2026-05-21
- **Blocker**: tooling/CI gap, not a code change
- **Last considered**: 2026-05-22 17:47 — defer to CI work
```

When an item is picked up and fixed, remove its entry in the same commit that lands the fix.

## Summary of obligations per cycle

```
✓ Part 1: dispatch 3-5 parallel reviewers (rotating lenses)
✓ Part 2: read backlog, pick 1-2 actionable items, dispatch fixers
✓ Verify each agent's claims with evidence
✓ Commit per logical fix
✓ Push after each green verification
✓ Append unfixed/deferred findings to deferred file
✓ Handle API 529 by retrying with sonnet, then skipping that lens
✓ Don't drift outside the named target directory
```

If a cycle fires and you only do Part 1 (because Part 2 felt empty), you've broken the contract. Read the deferred file every time, even if you think it hasn't changed.
