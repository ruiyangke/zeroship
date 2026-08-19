#!/usr/bin/env bash
# Census of distinct environment-variable names TEST code takes as INPUT.
#
# This is the instrument behind the claim that moving the suites onto the
# tracked TOML overlay REDUCED the name surface rather than renaming it. Run it
# on two revisions and diff; it is deterministic, takes no arguments, and
# prints one name per line to stdout.
#
# "Test code" is exactly three populations, chosen because each is a place a
# name can be introduced without any service ever declaring it:
#
#   1. Rust integration tests   crates/*/tests/**.rs, libs/*/tests/**.rs
#   2. Shared test helpers      crates/test-support/src/**.rs
#   3. Shell harnesses          tests/**.sh
#
# In-`src` `#[cfg(test)]` modules are NOT counted. A grep cannot tell which
# lines of a src file are inside one, and the clippy gate plus
# crates/core/tests/config_env_access_gate.rs already force every read there
# through a declared key, so those names are enumerable by construction and are
# not the sprawl this measures.
#
# A NAME COUNTS WHEN IT IS A KNOB -- something outside the script has to know
# to set. Two spellings, one per language:
#
#   shell   `${NAME:-default}` and its family (`:=`, `:?`, `-`, `=`, `?`).
#           That parameter expansion IS the "the environment may override
#           this" idiom; a harness that wants an input writes it this way.
#   rust    a name literal handed to an env accessor OR to one of the
#           declared-env macros. The macros are the dominant spelling and
#           missing them is the easy mistake: matching only `env::var("...")`
#           found 8 names here and none of the nine Postgres DSN names, because
#           every one of those is read as `zeroship_core::test_env!("...")`.
#
# WHAT IS DELIBERATELY NOT COUNTED, and why, because both alternatives were
# measured on this tree at the parent of this commit:
#
#   - every `$NAME` occurrence: 1537 names. That is a census of shell locals.
#   - `$NAME` read in a file that never assigns NAME: adds 126 names over the
#     definition above, and a hand read of them found essentially all noise --
#     `A`, `N`, `BODY`, `C1`, `CT`, `D`, `M`, `IDS` -- locals assigned through
#     spellings a regex misses (command substitution, `while read`, awk -v).
#     A handful of genuine no-default knobs live in there (MINIO_SECRET,
#     LAGO_KEY), so this arm undercounts slightly. It undercounts by the SAME
#     rule before and after, which is what a delta needs.
#
# One more failed design, kept because it is the subtle one: suppressing every
# name assigned by any sourced tests/lib/ file reported 12 names.
# tests/lib/scratch_db.sh assigns exactly the knobs
# (`PG_HOST="${PG_HOST:-...}"`), so the suppression ate the signal it was meant
# to clean. Read-with-default is never suppressed for that reason.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

find crates/*/tests libs/*/tests crates/test-support/src \
  -name '*.rs' -type f 2>/dev/null | sort > "$work/rust.list"
find tests -name '*.sh' -type f 2>/dev/null | sort > "$work/shell.list"

: > "$work/names"

# --- Rust ------------------------------------------------------------------
if [ -s "$work/rust.list" ]; then
  xargs -r -a "$work/rust.list" grep -hoE \
    '(test_env_os!|test_env!|declared_env_os!|declared_env!|read_config_env!|env::var_os|env::var|raw_var_os|raw_var)[[:space:]]*\([[:space:]]*(external|test|dev|cli|build|creator|platform)?[[:space:]]*,?[[:space:]]*"[A-Z][A-Z0-9_]*"' 2>/dev/null |
    grep -oE '"[A-Z][A-Z0-9_]*"' | tr -d '"' >> "$work/names"
fi

# --- Shell -----------------------------------------------------------------
if [ -s "$work/shell.list" ]; then
  xargs -r -a "$work/shell.list" grep -ohE '\$\{[A-Z][A-Z0-9_]*(:-|:=|:\?|-|=|\?)' 2>/dev/null |
    grep -oE '\{[A-Z][A-Z0-9_]*' | tr -d '{' >> "$work/names"
fi

# `OUT_DIR` is cargo's, not ours; the rest are shell/process standard.
sort -u "$work/names" | sed '/^$/d' | grep -vxE \
  'PATH|HOME|PWD|OLDPWD|SHELL|TERM|USER|LOGNAME|HOSTNAME|LANG|LC_[A-Z]*|TMPDIR|EDITOR|PAGER|IFS|PS[0-9]|BASH[A-Z_]*|FUNCNAME|LINENO|RANDOM|SECONDS|UID|EUID|PPID|OSTYPE|MACHTYPE|HOSTTYPE|SHLVL|COLUMNS|LINES|REPLY|PIPESTATUS|BASHPID|GROUPS|DIRSTACK|COMP_[A-Z]*|OPTARG|OPTIND|SHELLOPTS|BASHOPTS|OUT_DIR'
