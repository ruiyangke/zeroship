# s19 - tracked-secret gate. Working notes.

Scratch file for this branch. `NOTES.md` at the repo root belongs to the s16
auth-suite work and was NOT touched.

## 1. The gap, re-verified here (not taken on trust)

The brief said `tests/config_name_alignment_gate.sh:265`. That line number has
MOVED: 265 is now a comment line inside `check_compose_alias_equality`. The
claim itself is CORRECT at a different address:

- `tests/config_name_alignment_gate.sh:409` -- `check_ops_toml` joins with
  `awk -F'\t' -v t="$leaf" '$6==t {found=1} END{exit !found}'`. Column 6 is the
  TOML path. Column 3 is `class`. The class column is never read here.
- `tests/config_name_alignment_gate.sh:198-208` -- `toml_leaves` discards the
  VALUE half of every line. It prints `section.leaf` and nothing else. So no
  check in this script has ever seen an overlay value at all.

Consequence: a tracked overlay carrying `[control] master_key = "hunter2"` joins
cleanly (`control.master_key` IS a generated overlay path) and every check
passes.

Runtime side, confirmed by reading rather than by taking the amendment's word:
`crates/core/src/config/secrets.rs:343-344` -- `parse_secret_ref` returns
`SecretRef::Literal(raw)` for any value not prefixed `urn:`/`arn:`, from any
source including the file overlay. There is no refusal left to lean on.

## 2. The 2026-08-12 amendment: what else did it trade away

Amendment commit: `2d31a4bc5` "docs(proposals): put secrets in their component
table and allow literals". It touches ONE file (the proposal itself), 120+/24-.

Two changes in it:
- `[secrets]` table dissolved into component tables. The commit body says this
  "removes no check" and the diff agrees -- the reference-only rule was always
  per value in `obtain_secret`, never per section.
- The TOML-literal rejection, moved to the Section 4.7 tracked-file gate. This
  is the one that was never built.

It also ADDED a protection which is itself worth checking (see 2b).

### 2a. Did the same amendment trade away any OTHER runtime check? NO.

A clean negative, established rather than assumed. The amendment is a doc-only
commit; the CODE side of it is `94c7ba7dd` refactor(config)!: leave the secret
type admitting a literal and a file path only (2026-08-13). Four things were
deleted there. Only one was protection:

- `obtain_secret`'s literal rejection. THE trade. Verbatim at the amendment's
  parent: "config: {label}: a secret in the [secrets] config section must be a
  urn:/arn: reference, not a literal value", `std::process::exit(1)`. It really
  existed; it is really gone; check 8 is its replacement.
- The env-to-env arm `urn:zeroship:env:<VAR>`. A FEATURE, and deleting it
  removes the deployment alias hop. Strictly stronger.
- The Vault and AWS Secrets Manager reference forms. Both parsed and then
  always failed at boot with `BackendUnavailable`. Turning "parse, then refuse"
  into "refuse at parse" loses no protection.
- The flat `[secrets]` table. `#[serde(deny_unknown_fields)]` went WITH the
  fields into the component tables rather than being dropped: 21 occurrences in
  `crates/core/src/config/file.rs` before, 20 after, and the one lost is the
  deleted `[secrets]` struct's own attribute. The misspelled-key refusal is
  intact.

The malformed-reference refusal (a `urn:`/`arn:` value that is not a
recognized reference) also survives, at `crates/core/src/config/secrets.rs:326`
and `:481-489`.

The other 2026-08-12 commit that reads like this pattern is `972798057`
docs(proposals): record the two deletions the six-behaviour table missed. It
belongs to a DIFFERENT proposal (`2026-08-12-secure-by-default-config.md`) and
its two deletions - AUTH_INSECURE_DEV and the flag-gated localhost
platform-issuer default - are RELAXATIONS removed. That is the opposite trade.

### 2b. The amendment's OTHER unlanded promise, already owned

Section 4.7 also ADDED: "Runtime permission check. When the overlay contains a
secret literal and is group- or world-readable, startup refuses." That is not
this gate's subject and is NOT built here. It is the subject of the queued
secret-file-permissions task, live in `.worktrees/s24`, so it has an owner
and is not an orphan. Recorded so the next reader does not re-derive it.

## 3. Scope decision for the gate

See the gate's own header comment (check 8) for the durable version. The short
form: the TOML arm lands, the compose `environment:` arm does not, and the
reason is measured rather than asserted -- six secret-classed compose keys
carry an inline DSN default with userinfo (lines 257, 431, 435, 517, 610, 731),
and the only non-arbitrary fix makes all six required inputs that
`zeroship dev init` does not write. An allowlist for them would be an allowlist
for the gate's own subject.

## 4. Mutation proof, four runs

Mutation: three lines appended to the TRACKED `deploy/ops/zeroship.toml`.

```text
[control]
master_key = "b7c1e2f9a4d68035c1ff2ab90e5d7643"
```

Applied-check, because an unapplied edit and a non-discriminating gate print
the same thing: md5 `a47f4b0f...` before, `d7cf50bc...` after, `diff` showing
`68a69,71`. After the revert the md5 is `a47f4b0f...` again and `diff` against
the saved original is empty.

| run | gate | mutation | result |
| --- | --- | --- | --- |
| 1 | `main:tests/config_name_alignment_gate.sh` | applied | exit 0, 11 passed 0 failed |
| 2 | this branch | applied | exit 1, 11 passed 1 failed |
| 3 | this branch `--self-test` | applied | exit 1, 12 passed 1 failed |
| 4 | this branch, both modes | reverted | exit 0, 12 and 13 passed, 0 failed |

Run 1 is the load-bearing one. The PRE-FIX gate passes the planted secret with
a clean 11/0, which is the gap stated as an observation instead of a reading.
Runs 2 and 4 differ in one variable each, so the RED is attributable to the
literal and the GREEN to its removal.

Run 2's output, verbatim:

```text
=== 8. No tracked file carries a plaintext secret ===
  deploy/ops/zeroship.toml:71 control.master_key is secret-classed and holds a plaintext literal;
      a tracked file may hold only a urn:/arn: reference
FAIL: tracked secret literals: 1 secret-classed leaf/leaves hold a plaintext literal in a tracked file
```

The value is deliberately NOT echoed. This runs in CI, and a check that
publishes the material it just found would be its own disclosure.
