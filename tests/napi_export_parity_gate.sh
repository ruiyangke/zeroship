#!/usr/bin/env bash
#
# Every symbol the napi addon DECLARES in index.d.ts must be EXPORTED by
# index.js.
#
# THE DEFECT THIS EXISTS FOR, shipped 2026-08-28 and caught by accident.
# `2690b5a16` added the `baseline` verb across the whole stack. Its napi export
# landed in `index.d.ts` (the TypeScript declaration) and NOT in `index.js` (the
# CommonJS re-export). The committed tree therefore:
#
#   - compiled:      the Rust side was complete
#   - type-checked:  TypeScript trusts the .d.ts and never reads index.js
#   - tested green:  `cargo test` does not cross the napi boundary, and the
#                    authoring agent's own worktree HAD the built artifact, so
#                    its runs passed against a file the commit did not carry
#
# and would have failed at the first `zero-migrate baseline` with
# `nativeBinding.baselineIr is not a function`. It was found only because a
# routine `git status` showed one unstaged line.
#
# The class is generated-artifact skew: two files emitted from one source, of
# which only one is consulted by any check. `index.d.ts` is the contract every
# tool reads; `index.js` is the code that actually runs. Nothing compared them.
#
# WHY A GATE AND NOT A REGENERATION STEP. Regenerating on every build would hide
# the skew rather than report it - the tree would be correct locally and the
# COMMIT would still be wrong, which is exactly the shape that shipped. This
# fails on the committed bytes.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
# shellcheck source=lib/gate_arms.sh
. tests/lib/gate_arms.sh

ADDON_DIR=crates/zeroship-migrate-node
DTS="$ADDON_DIR/index.d.ts"
JS="$ADDON_DIR/index.js"

gate_arms_init napi_export_parity

fail=0

if [ ! -f "$DTS" ] || [ ! -f "$JS" ]; then
  # NOT a skip. These are tracked files; their absence is the loudest possible
  # form of the thing this gate checks, and a gate that shrugs at a missing
  # subject is the vacuous-arm failure `gate_arms.sh` exists to prevent.
  echo "REFUSING: $DTS or $JS is absent - the addon artifacts are tracked, so"
  echo "  this is not a 'not built yet' condition."
  exit 1
fi

# `export declare function foo(` in the .d.ts is the addon's public surface.
# Types, interfaces and enums are declaration-only and correctly absent from
# index.js, so they are not part of the comparison.
declared=$(grep -oE '^export declare function [A-Za-z_][A-Za-z0-9_]*' "$DTS" 2>/dev/null \
  | awk '{print $4}' | sort -u)
declared_n=$(printf '%s\n' "$declared" | grep -c . || true)

exported=$(grep -oE 'module\.exports\.[A-Za-z_][A-Za-z0-9_]*' "$JS" 2>/dev/null \
  | sed 's/module\.exports\.//' | sort -u)
exported_n=$(printf '%s\n' "$exported" | grep -c . || true)

# ARM 1: every declared function is exported. The number ruled on is the count
# of DECLARED functions - if the .d.ts is ever emptied, this arm examines 0 and
# the floor catches it rather than printing the green a clean tree prints.
missing=$(comm -23 <(printf '%s\n' "$declared") <(printf '%s\n' "$exported") | grep -c . || true)
gate_arm declared_functions_are_exported "$declared_n" 10
if [ "$missing" -ne 0 ]; then
  echo "FAIL: $missing function(s) declared in $DTS but not exported by $JS:"
  comm -23 <(printf '%s\n' "$declared") <(printf '%s\n' "$exported") | sed 's/^/    /'
  echo "  A caller reaching one of these gets 'is not a function' at runtime,"
  echo "  after a clean compile and a clean typecheck."
  fail=1
fi

# ARM 2: the reverse. An export with no declaration is invisible to every
# TypeScript consumer, so it is dead surface or a stale hand-edit. Ruled on:
# the count of EXPORTED names.
extra=$(comm -13 <(printf '%s\n' "$declared") <(printf '%s\n' "$exported") | grep -c . || true)
gate_arm exports_are_declared "$exported_n" 10
if [ "$extra" -ne 0 ]; then
  echo "FAIL: $extra name(s) exported by $JS with no declaration in $DTS:"
  comm -13 <(printf '%s\n' "$declared") <(printf '%s\n' "$exported") | sed 's/^/    /'
  fail=1
fi

gate_arms_finish || fail=1

if [ "$fail" -eq 0 ]; then
  echo "napi export parity: $declared_n declared, $exported_n exported, in sync"
fi
exit "$fail"
