# shellcheck shell=bash
# ============================================================================
# ONE definition of "is this module compiled out of every shipped binary".
#
# A Rust module can be gated where it is DECLARED rather than where it is
# defined - the attribute lives in the PARENT file:
#
#     // lib.rs:259
#     #[cfg(any(test, feature = "test-helpers"))]
#     pub mod drop_namespace;
#
# `drop_namespace.rs` carries no cfg of its own, so any in-file production
# region filter reads all of it as shipped code. Every instrument that rules on
# "what does a shipped binary contain" needs this predicate, and until
# 2026-09-04 FOUR of them carried their own copy of it:
#
#     tests/vendor_embedding_gate.sh        buggy
#     tests/lib/pub_fence_census.sh         buggy
#     tests/lib/tier_signature_census.sh    buggy
#     tests/decision_four_gate.sh           corrected, and only that one
#
# THE BUG THE THREE COPIES SHARED, and it is an attribute-versus-prose
# confusion inside the helper itself. The backward walk's `case` tested
#
#     *"#[cfg("*)   ...   ;;          # arm 1: is this a cfg attribute?
#     "#["*|*"//"*|"")  i=$((i - 1))  # arm 2: attribute run / comment / blank
#
# with arm 1 FIRST and matching anywhere in the line. A COMMENT that merely
# QUOTES `#[cfg(` therefore matched arm 1, `*test*` matched the quoted `test`,
# and the module was recorded as test-gated. Careful documentation switched the
# instrument off.
#
# Measured 2026-09-04 at be17704ae, over the four roots the vendor gate scans:
# exactly two files were wrongly skipped, and BOTH by prose written to explain a
# visibility decision -
#
#   crates/zeroship-data-engine/src/backend/mod.rs
#     via crates/zeroship-data-engine/src/lib.rs:96
#     "// ... and one `#[cfg(test)]` conformance assertion. The"
#   crates/zeroship-data-engine/src/crud/unmask.rs
#     via crates/zeroship-data-engine/src/crud/mod.rs:80
#     "// `#[cfg(test)]` in `v8_classes/mod.rs`. Narrowing this to `pub(crate)` in"
#
# The second was written the same day, by 7611d6213, recording a measurement of
# why `crud::unmask` must stay `pub`. Neither file names a vendor in production,
# so nothing was concealed - but the two gates DISAGREED about which files exist
# to rule on, which is how a census stops meaning anything.
#
# WHY NOT SIMPLY REORDER THE ARMS. Testing the comment arm first misclassifies a
# REAL attribute that carries a trailing comment:
#
#     #[cfg(test)] // only the tests need this
#     pub mod probe;
#
# That line contains `//`, so a comment-first `case` would walk straight past a
# genuine gate. Both orderings are wrong because the two arms OVERLAP. The fix
# is to make them disjoint: trim leading whitespace, then test whether the line
# STARTS with `#[`. An attribute begins its line (rustfmt emits one per line at
# the item's indentation); a comment begins with `/`. No ordering can
# reintroduce the defect once the patterns cannot both match. Every case in
# `module_gating_self_test` below is a control for one half of that.
#
# WHAT THIS CANNOT DO, stated so nobody reads it as complete:
#
#   - It does not evaluate cfg algebra. Any `not(` makes it treat the arm as
#     SHIPPED, which is deliberate: `"test-helpers"` CONTAINS `test`, and
#     matching that naively is how a sibling census went to zero rows while
#     printing that as calmly as a real number.
#   - A multi-line attribute defeats it. `#[cfg(any(\n test,\n ...))]` presents
#     `))]` on the line above the declaration, which is neither an attribute nor
#     a comment, so the walk stops and the module reads as SHIPPED. False
#     negative: the file gets scanned, which is noise rather than silence.
#   - `#[cfg(test)]`, then a PLAIN `//` comment, then `mod x;` also reads as
#     shipped, for the same reason and in the same safe direction. Doc comments
#     (`///`, `//!`) and further attributes do continue the walk - those really
#     are part of an item's attribute run.
#   - A line inside a `/* ... */` block comment that happens to begin with
#     `#[cfg(test)]` would be read as an attribute. No such line exists in the
#     tree; the residual is recorded rather than defended against.
#   - It is a TEXT walk, not a compiler. It answers how the declaration is
#     SPELLED, never whether the module is reachable.
# ============================================================================

# module_is_test_gated <file.rs> <root>...
#
# True when EVERY `mod <name>;` declaration of this file's module, across all
# the given roots, carries a test-ish cfg.
#
# EVERY declaration must be gated, not merely one. The data-plane crates declare
# most modules through a two-arm visibility ladder -
#
#     #[cfg(not(feature = "test-helpers"))] pub(crate) mod exec;
#     #[cfg(feature = "test-helpers")]      pub       mod exec;
#
# - so "any gated declaration" would exclude nearly the whole crate.
#
# The roots are ARGUMENTS. The four copies this replaces each hard-coded a
# different search root (`$ROOTS`, `$ROOT`, `$SRC`, `.`), and a shared helper
# that picked one of them would silently change what the other three rule on.
module_is_test_gated() {
  local file="$1"
  shift
  local name decls gated hit decl_file decl_line prev trimmed i
  name=$(basename "$file" .rs)
  # `foo/mod.rs` is declared as `mod foo;`, not `mod mod;`.
  if [ "$name" = "mod" ]; then
    name=$(basename "$(dirname "$file")")
  fi
  [ -z "$name" ] && return 1
  [ "$#" -ge 1 ] || return 1

  decls=0
  gated=0
  while IFS= read -r hit; do
    decl_file="${hit%%:*}"
    decl_line="${hit#*:}"
    decl_line="${decl_line%%:*}"
    decls=$((decls + 1))
    i=$((decl_line - 1))
    while [ "$i" -ge 1 ]; do
      prev=$(sed -n "${i}p" "$decl_file")
      # Leading whitespace only. What is left either BEGINS with an attribute or
      # is not one; the arms below cannot both match.
      trimmed="${prev#"${prev%%[![:space:]]*}"}"
      case "$trimmed" in
        "#[cfg("*)
          case "$trimmed" in
            *"not("*) break ;;
            *test*) gated=$((gated + 1)); break ;;
            *) break ;;
          esac
          ;;
        "#["*|"///"*|"//!"*|"") i=$((i - 1)) ;;
        *) break ;;
      esac
    done
  done < <(grep -rn -E "^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${name}[[:space:]]*;" "$@")

  [ "$decls" -gt 0 ] && [ "$decls" -eq "$gated" ]
}

# module_gating_self_test
#
# The detector's own positive/negative controls, on a synthetic tree. Every case
# changes ONE variable against its neighbour; a case with no partner proves only
# that the regex ran.
#
# Cases 2 and 12 are the REGRESSION: they fail against the pre-2026-09-04
# helper and pass against this one. Case 3 is what stops the obvious "reorder
# the arms" fix - it fails against a comment-first ordering. Keep all three.
module_gating_self_test() {
  local tmp status=0
  tmp="$(mktemp -d)"

  mkdir -p "$tmp/src/sub"
  cat > "$tmp/src/lib.rs" <<'RS'
#[cfg(test)]
pub mod gated;
// A prose comment that quotes `#[cfg(test)]` while explaining something else.
pub mod prose;
#[cfg(test)] // only the tests need this
pub mod trailing;
#[cfg(not(feature = "test-helpers"))]
pub mod shipped;
#[cfg(any(test, feature = "test-helpers"))]
pub mod helpers;
#[cfg(test)]
/// A doc comment is part of the item's attribute run.
pub mod docrun;
#[cfg(test)]
// A plain comment is NOT, and stops the walk. Documented false negative.
pub mod plaincomment;
#[cfg(test)]
#[allow(dead_code)]
pub mod attrrun;
pub mod ungated;
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod ladder;
#[cfg(feature = "test-helpers")]
pub mod ladder;
#[cfg(test)]
pub mod sub;
/// See `#[cfg(test)]` for how this is compiled out.
pub mod docquote;
RS
  cat > "$tmp/src/indent.rs" <<'RS'
mod outer {
    #[cfg(test)]
    pub mod indented;
}
RS
  local f
  for f in gated prose trailing shipped helpers docrun plaincomment attrrun \
           ungated ladder docquote indented; do
    : > "$tmp/src/$f.rs"
  done
  : > "$tmp/src/sub/mod.rs"

  _mg_case() {   # <label> <expect gated: yes|no> <file>
    local label="$1" expect="$2" path="$3" got=no
    module_is_test_gated "$path" "$tmp/src" && got=yes
    if [ "$got" = "$expect" ]; then
      echo "  ok   module gating: $label"
    else
      echo "  FAIL module gating: $label - expected gated=$expect, got gated=$got"
      status=1
    fi
  }

  _mg_case "a bare #[cfg(test)] gates the module"                 yes "$tmp/src/gated.rs"
  _mg_case "a COMMENT quoting #[cfg(test)] does NOT gate it"      no  "$tmp/src/prose.rs"
  _mg_case "a real attribute with a trailing comment DOES gate"   yes "$tmp/src/trailing.rs"
  _mg_case "a #[cfg(not(feature = ...))] arm is SHIPPED"          no  "$tmp/src/shipped.rs"
  _mg_case "any(test, feature = \"test-helpers\") gates"          yes "$tmp/src/helpers.rs"
  _mg_case "a doc comment continues the attribute run"            yes "$tmp/src/docrun.rs"
  _mg_case "a plain comment stops the walk (safe false negative)" no  "$tmp/src/plaincomment.rs"
  _mg_case "a second attribute continues the run"                 yes "$tmp/src/attrrun.rs"
  _mg_case "an undecorated declaration is not gated"              no  "$tmp/src/ungated.rs"
  _mg_case "a ladder with one shipped arm is not gated"           no  "$tmp/src/ladder.rs"
  _mg_case "foo/mod.rs is keyed on the DIRECTORY name"            yes "$tmp/src/sub/mod.rs"
  _mg_case "a DOC comment quoting #[cfg(test)] does NOT gate"     no  "$tmp/src/docquote.rs"
  _mg_case "an indented #[cfg(test)] still gates"                 yes "$tmp/src/indented.rs"

  # A file whose module is declared NOWHERE must not read as gated: `decls > 0`
  # is what separates "every declaration is gated" from "there were none".
  : > "$tmp/src/orphan.rs"
  _mg_case "a module with no declaration at all is not gated"     no  "$tmp/src/orphan.rs"

  unset -f _mg_case
  rm -rf "$tmp"
  return "$status"
}
