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
#   crates/zeroship-data-orm/src/backend/mod.rs
#     via crates/zeroship-data-orm/src/lib.rs:96
#     "// ... and one `#[cfg(test)]` conformance assertion. The"
#   crates/zeroship-data-orm/src/protection/unmask.rs
#     via crates/zeroship-data-orm/src/crud/mod.rs:80
#     "// `#[cfg(test)]` in `v8_classes/mod.rs`. Narrowing this to `pub(crate)` in"
#
# The second was written the same day, by 7611d6213, recording a measurement of
# why `protection::unmask` must stay `pub`. Neither file names a vendor in production,
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
#   - Inside the attribute it matches `test` as a TOKEN, not as a substring, and
#     strips a trailing `//` comment first. Both are the SAME defect this file
#     was written for, one level down: `#[cfg(feature = "latest")]` and
#     `#[cfg(unix)] // the test harness is the only caller` were each read as
#     test-gated until 2026-09-04. See `_module_gating_cfg_names_test` for the
#     measurement that says this was latent rather than live.
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
# True when every matching declaration carries a test-ish cfg or inherits one
# through its ordinary parent-file layout. Resolvable declarations of unrelated
# files do not participate; unresolved declarations remain conservative.
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
# _module_gating_cfg_names_test <cfg attribute text, comment-stripped>
#
# True when the cfg predicate names `test` AS A TOKEN, or names a feature whose
# name is test-ish: `test`, `testing`, `test-*`/`test_*`, `*-test`/`*_test`.
#
# WHY A TOKEN AND NOT A SUBSTRING. The predicate here was `*test*` against the
# whole line until 2026-09-04. `latest`, `fastest`, `protest` and `attest` all
# contain `test`, and any of them as a feature name would have made an ordinary
# shipped module read as compiled-out - which is the direction that makes a
# census QUIETER, not louder. Measured the same day over crates/ and libs/:
# every cfg attribute that today decorates a `mod` declaration and reaches this
# arm is genuinely test-related (`#[cfg(test)]` x19, `feature = "test-helpers"`
# x7, `any(test, feature = "test-helpers")` x4, `any(test, feature = "testing")`
# x2), and no cfg attribute line anywhere in either tree carries a trailing `//`
# comment. So this was LATENT, not live, and is fixed on the reasoning that the
# whole file exists because a latent prose-versus-attribute confusion became
# live the day somebody wrote a careful comment.
#
# It does NOT evaluate cfg algebra; the `not(` arm above still short-circuits.
_module_gating_cfg_names_test() {
  local tok
  for tok in ${1//[^A-Za-z0-9_-]/ }; do
    case "$tok" in
      test|testing|test[-_]*|*[-_]test) return 0 ;;
    esac
  done
  return 1
}

module_is_test_gated() {
  local file="$1"
  shift
  local name decls gated hit decl_file decl_line prev trimmed code i before
  local canonical resolved stack="${_module_gating_stack:-}"
  [ -f "$file" ] || return 1
  canonical=$(realpath "$file") || return 1
  case "$stack" in *$'\n'"$canonical"$'\n'*) return 1 ;; esac
  local _module_gating_stack="$stack"$'\n'"$canonical"$'\n'
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
    resolved=""
    if resolved=$(_module_gating_ordinary_child "$decl_file" "$decl_line" "$name"); then
      [ "$resolved" = "$canonical" ] || continue
    fi
    decls=$((decls + 1))
    before=$gated
    i=$((decl_line - 1))
    while [ "$i" -ge 1 ]; do
      prev=$(sed -n "${i}p" "$decl_file")
      # Leading whitespace only. What is left either BEGINS with an attribute or
      # is not one; the arms below cannot both match.
      trimmed="${prev#"${prev%%[![:space:]]*}"}"
      case "$trimmed" in
        "#["*)
          # Drop a trailing `//` comment before reading the attribute. This file
          # exists because prose that QUOTED an attribute was read as one; prose
          # that merely MENTIONS `test` beside a real attribute is the same
          # mistake one level down, and `#[cfg(unix)] // the test harness is the
          # only caller` used to be recorded as test-gated.
          code="${trimmed%%//*}"
          case "$code" in
            "#[cfg("*)
              case "$code" in
                *"not("*) break ;;
                *)
                  if _module_gating_cfg_names_test "$code"; then
                    gated=$((gated + 1))
                  fi
                  break
                  ;;
              esac
              ;;
            *) i=$((i - 1)) ;;
          esac
          ;;
        "///"*|"//!"*|"") i=$((i - 1)) ;;
        *) break ;;
      esac
    done
    if [ "$gated" -eq "$before" ] &&
       [ "$resolved" = "$canonical" ] &&
       module_is_test_gated "$decl_file" "$@"; then
      gated=$((gated + 1))
    fi
  done < <(grep -rn -E "^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${name}[[:space:]]*;" "$@")

  [ "$decls" -gt 0 ] && [ "$decls" -eq "$gated" ]
}

# Recognize ordinary file modules before following an inherited gate. Inline
# declarations and files containing path overrides stay conservative; this
# helper does not attempt to resolve their Rust module paths.
_module_gating_ordinary_child() {
  local parent="$1" line="$2" child_name="$3" parent_dir name declaration child
  declaration=$(sed -n "${line}p" "$parent")
  case "$declaration" in [[:space:]]*) return 1 ;; esac
  if rg -q '^[[:space:]]*#\[path[[:space:]]*=' "$parent"; then return 1; fi
  parent_dir=$(dirname "$parent")
  name=$(basename "$parent" .rs)
  case "$name" in
    lib|main|mod) ;;
    *) parent_dir="$parent_dir/$name" ;;
  esac
  child="$parent_dir/$child_name.rs"
  if [ -f "$child" ]; then
    [ ! -f "$parent_dir/$child_name/mod.rs" ] || return 1
  else
    child="$parent_dir/$child_name/mod.rs"
    [ -f "$child" ] || return 1
  fi
  realpath "$child"
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
  local tmp status=0 checked=0
  MODULE_GATING_CHECKED=0
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
#[cfg(feature = "latest")]
pub mod latest;
#[cfg(unix)] // the test harness is the only caller
pub mod unixonly;
#[cfg(feature = "integration-test")]
pub mod inttest;
RS
  cat > "$tmp/src/indent.rs" <<'RS'
mod outer {
    #[cfg(test)]
    pub mod indented;
}
RS
  cat >> "$tmp/src/lib.rs" <<'RS'
#[cfg(test)]
mod test_parent;
mod shipped_parent;
#[cfg(test)]
mod dir_parent;
#[cfg(test)]
mod path_parent;
#[cfg(test)]
mod inline_parent;
RS
  mkdir -p "$tmp/src/test_parent/intermediate" "$tmp/src/shipped_parent" "$tmp/src/dir_parent" "$tmp/src/unrelated" "$tmp/src/recursive"
  mkdir -p "$tmp/src/test_parent/dir_child" "$tmp/src/path_parent" "$tmp/src/inline_parent"
  cat > "$tmp/src/test_parent.rs" <<'RS'
mod inherited;
mod intermediate;
#[cfg(not(feature = "test-helpers"))]
mod inherited_ladder;
mod collision;
mod dir_child;
RS
  printf 'mod leaf;\n' > "$tmp/src/test_parent/intermediate.rs"
  printf 'mod shipped_child;\nmod collision;\n' > "$tmp/src/shipped_parent.rs"
  printf 'mod nested_dir;\n' > "$tmp/src/dir_parent/mod.rs"
  printf 'mod lib;\n' > "$tmp/src/recursive/lib.rs"
  printf '#[path = "elsewhere.rs"]\nmod path_child;\n' > "$tmp/src/path_parent.rs"
  printf 'mod inner {\n    mod inline_child;\n}\n' > "$tmp/src/inline_parent.rs"
  local child
  for child in test_parent/inherited test_parent/intermediate/leaf test_parent/inherited_ladder \
               test_parent/collision shipped_parent/collision shipped_parent/shipped_child \
               dir_parent/nested_dir unrelated/inherited test_parent/dir_child/mod \
               path_parent/path_child inline_parent/inline_child; do
    : > "$tmp/src/$child.rs"
  done
  local f
  for f in gated prose trailing shipped helpers docrun plaincomment attrrun \
           ungated ladder docquote indented latest unixonly inttest; do
    : > "$tmp/src/$f.rs"
  done
  : > "$tmp/src/sub/mod.rs"

  _mg_case() {   # <label> <expect gated: yes|no> <file>
    local label="$1" expect="$2" path="$3" got=no
    checked=$((checked + 1))
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

  # The `test` TOKEN, not the substring. These three are the 2026-09-04
  # regression: the first two pass against a `*test*` case pattern only by
  # accident of nothing in the tree being spelled that way yet.
  _mg_case "feature \"latest\" merely CONTAINS test - not gated"   no  "$tmp/src/latest.rs"
  _mg_case "a comment mentioning the test harness does NOT gate"  no  "$tmp/src/unixonly.rs"
  _mg_case "feature \"integration-test\" DOES gate"                yes "$tmp/src/inttest.rs"

  # A file whose module is declared NOWHERE must not read as gated: `decls > 0`
  # is what separates "every declaration is gated" from "there were none".
  : > "$tmp/src/orphan.rs"
  _mg_case "a module with no declaration at all is not gated"     no  "$tmp/src/orphan.rs"

  _mg_case "a child inherits its parent's test gate" yes "$tmp/src/test_parent/inherited.rs"
  _mg_case "a descendant inherits through intermediate files" yes "$tmp/src/test_parent/intermediate/leaf.rs"
  _mg_case "a child visibility condition cannot reopen a gated parent" yes "$tmp/src/test_parent/inherited_ladder.rs"
  _mg_case "a mod.rs parent carries its gate to children" yes "$tmp/src/dir_parent/nested_dir.rs"
  _mg_case "a mod.rs child inherits its parent's gate" yes "$tmp/src/test_parent/dir_child/mod.rs"
  _mg_case "a shipped parent's child remains shipped" no "$tmp/src/shipped_parent/shipped_child.rs"
  _mg_case "an unrelated same-named file does not inherit a gate" no "$tmp/src/unrelated/inherited.rs"
  _mg_case "an unrelated shipped module does not cancel an inherited gate" yes "$tmp/src/test_parent/collision.rs"
  _mg_case "an unrelated test module does not exempt shipped code" no "$tmp/src/shipped_parent/collision.rs"
  _mg_case "a recursive declaration cannot exempt itself" no "$tmp/src/recursive/lib.rs"
  _mg_case "a path override does not imply an ordinary parent" no "$tmp/src/path_parent/path_child.rs"
  _mg_case "an inline declaration does not imply an ordinary parent" no "$tmp/src/inline_parent/inline_child.rs"

  MODULE_GATING_CHECKED=$checked
  unset -f _mg_case
  rm -rf "$tmp"
  return "$status"
}
