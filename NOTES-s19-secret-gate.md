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

It also ADDED a protection which is itself worth checking (see section 4).

## 3. Scope decision for the gate

Filled in after measuring; see the gate's own header comment for the durable
version.
