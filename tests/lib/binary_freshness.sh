#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Are the dev-side and deployed-side binaries built from the same source?
#
# A dev-vs-deployed harness is the one kind of test that cannot answer this for
# itself. `pnpm dev` runs `target/release/zeroship`; the deployed side runs
# zeroship-worker, -gate and -control. They are separate binaries, so a partial
# rebuild leaves the two sides at different commits and the harness dutifully
# reports the version skew as a backend divergence.
#
# That is not hypothetical. Walking scenario 5, the worker predated 3e7e5e387
# ("page env.storage.list") and still returned a bare array where the SDK now
# expects `{entries, cursor}`. Every list operation "diverged" with
# `TypeError: r.entries.map is not a function` -- which reads exactly like an S3
# list defect, and `Array.prototype.entries` really is a function, so the error
# text is convincing. It cost a full debugging cycle and was separated only by
# pointing the DEV server at the same MinIO, which exonerated the backend.
#
# WARNS, does not fail, by default. mtime is a weak proxy for provenance: on a
# fresh clone every source file is newer than a binary that does not exist yet,
# and a source file touched by a rebase is newer without being different. A
# check that refuses on that would block runs whose staleness is irrelevant to
# the surfaces under test. Set ZS_FRESHNESS_STRICT=1 to make it refuse instead
# -- worth doing in CI, where a stale-binary run produces confident wrong
# findings and nobody is watching the warnings.
#
# Usage:
#   source "$ROOT/tests/lib/binary_freshness.sh"
#   zs_check_binary_freshness "$ROOT" "$BIN" \
#     "crates/zeroship-kv-v8/src crates/runtime/src packages/kv/src" \
#     "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision"
#
# Returns 0 when everything is fresh (or only warnings are wanted), 1 when
# something is stale AND ZS_FRESHNESS_STRICT=1, and 2 when it could not answer
# the question -- a missing binary, or source directories that matched no files
# at all. That last case is deliberate: a check that silently passes because its
# own paths were wrong is the failure mode this repo has shipped twice.
#
# BASH ONLY. The directory and binary lists are space-separated strings and rely
# on word splitting, which zsh does not do for unquoted expansions. Sourcing
# this from an interactive zsh makes every list collapse to one nonexistent path
# and the function returns 2 -- verified by doing it. That is the intended
# behaviour rather than a wart: the cannot-answer arm is what turns a shell
# mismatch into a loud refusal instead of a green run over nothing.
# ---------------------------------------------------------------------------

zs_check_binary_freshness() {
  local root="$1" bin="$2" srcdirs="$3" binaries="$4"
  local strict="${ZS_FRESHNESS_STRICT:-0}"

  # Published for the caller to RE-REPORT beside its own findings. Reset on
  # every call so an all-fresh run cannot inherit a previous run's count.
  # See the note above `stale_names` for the run that motivated this.
  ZS_FRESHNESS_STALE_COUNT=0
  ZS_FRESHNESS_STALE_NAMES=""

  local existing=() missing=() d
  for d in $srcdirs; do
    if [ -d "$root/$d" ]; then existing+=("$root/$d"); else missing+=("$d"); fi
  done
  if [ "${#existing[@]}" -eq 0 ]; then
    echo "  FRESHNESS: none of the named source directories exist: $srcdirs" >&2
    echo "             The check cannot run, which is not the same as passing." >&2
    return 2
  fi
  # A PARTIALLY missing list is the dangerous case, and until 2026-08-27 it was
  # accepted silently: only the all-missing arm above refused, so a list whose
  # paths had been renamed out from under it kept scanning whatever survived and
  # printed exactly what a healthy run prints.
  #
  # That was live. The 2026-08-26 crate-directory rename (crates/plugin-db ->
  # crates/zeroship-data-v8, and seven siblings) left e2e_dev_vs_deployed_db.sh
  # naming eight directories of which only TWO still existed. The guard whose
  # whole job is "did you rebuild after touching plugin-db" had stopped looking
  # at plugin-db, and reported fresh either way.
  #
  # Refusing rather than warning, because this function exists to make staleness
  # loud, and a guard quietly covering a quarter of its subject IS the failure it
  # was written to prevent.
  if [ "${#missing[@]}" -gt 0 ]; then
    local _total=$(( ${#existing[@]} + ${#missing[@]} ))
    echo "  FRESHNESS: ${#missing[@]} of $_total named source directories do not exist:" >&2
    printf '               %s\n' "${missing[@]}" >&2
    echo "             Scanning only the survivors would report FRESH for a change" >&2
    echo "             under any missing path. Fix the caller's list." >&2
    return 2
  fi

  # `-printf | sort -nr` rather than `xargs ls -t`: the runtime crate carries a
  # ~1GB gitignored WPT checkout, and a multi-batch xargs would report only the
  # newest file of the LAST batch.
  local newest_src
  newest_src=$(find "${existing[@]}" \( -name '*.rs' -o -name '*.ts' \) \
    -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -1 | cut -d' ' -f2-)
  if [ -z "$newest_src" ]; then
    echo "  FRESHNESS: matched no .rs or .ts files under: $srcdirs" >&2
    echo "             The check cannot run, which is not the same as passing." >&2
    return 2
  fi

  # Name the CRATE, not just the file. "worker is older than mod.rs" tells a
  # reader nothing; "older than crates/zeroship-kv-v8" tells them what to rebuild.
  local rel crate
  rel="${newest_src#"$root"/}"
  crate="$(printf '%s' "$rel" | cut -d/ -f1-2)"

  # `stale_names` exists because a warning is only useful WHERE THE READER IS.
  # Measured 2026-08-10 on tests/e2e_dev_vs_deployed_db.sh: this check fired
  # correctly, named `dispatch.rs`, and warned that six binaries predated it.
  # The run then reported a three-row dev-vs-deployed divergence, two rows of
  # which were precisely that skew -- formatted identically to the one real
  # backend divergence. The warning was at line 1 of a 160-line log; the diff
  # was at the bottom, which is where a reader looks. The warning was not read,
  # and the skew was re-derived by hand from binary mtimes instead.
  #
  # So the verdict is published rather than only echoed, and callers that print
  # a diff are expected to repeat it there. Names as well as a count: "something
  # was stale" does not tell a reader WHICH side of the comparison to distrust.
  local stale=0 b missing=0 stale_names=""
  for b in $binaries; do
    if [ ! -x "$bin/$b" ]; then
      echo "  FRESHNESS: missing $bin/$b -- see the prereqs in the calling script's header" >&2
      missing=$((missing+1))
      continue
    fi
    if [ "$newest_src" -nt "$bin/$b" ]; then
      stale=$((stale+1))
      stale_names="${stale_names:+$stale_names }$b"
      echo "  WARN $b is OLDER than $crate ($(basename "$newest_src"))"
    fi
  done

  ZS_FRESHNESS_STALE_COUNT="$stale"
  ZS_FRESHNESS_STALE_NAMES="$stale_names"
  ZS_FRESHNESS_STALE_CRATE="$crate"

  [ "$missing" -gt 0 ] && return 2

  if [ "$stale" -gt 0 ]; then
    echo "       $stale binar$([ "$stale" -eq 1 ] && echo y || echo ies) predate $crate. Rebuild, or a"
    echo "       difference below may be version skew between the two sides rather"
    echo "       than a real dev-vs-deployed divergence."
    [ "$strict" = "1" ] && return 1
    return 0
  fi

  echo "  ok   both sides built after the newest source under $crate"
  return 0
}

# ---------------------------------------------------------------------------
# Re-report the staleness verdict AT THE POINT OF A FINDING.
#
# Call this from inside a diff/divergence branch, after
# `zs_check_binary_freshness` has run. Prints nothing when everything was
# fresh, so it is safe to call unconditionally.
#
# Why it is a function and not three copies: the three dev-vs-deployed
# harnesses that print a diff all need it, and a copied block is the same
# hand-mirrored shape that has already produced several defects here -- one
# copy gets updated, the others quietly stop matching, and nothing fails.
zs_report_staleness_here() {
  [ "${ZS_FRESHNESS_STALE_COUNT:-0}" -gt 0 ] || return 0
  echo "  !! ${ZS_FRESHNESS_STALE_COUNT} BINARY(S) WERE STALE when this ran:" \
       "${ZS_FRESHNESS_STALE_NAMES}"
  echo "     (older than ${ZS_FRESHNESS_STALE_CRATE:-a changed crate})."
  echo "     Some rows above may be VERSION SKEW between the two sides rather"
  echo "     than a backend divergence -- they are indistinguishable in this"
  echo "     diff. Rebuild and re-run before believing any row."
  echo ""
}

# ---------------------------------------------------------------------------
# The same question for a BUILT JS ARTEFACT rather than a Rust binary.
#
# `zs_check_binary_freshness` tests `-x`, which is right for binaries and wrong
# here: `packages/vite-plugin/dist/index.js` is not executable, so passing it to
# that function reports it MISSING and returns 2. Rather than relax the `-x`
# (it is load-bearing there) or copy the logic, this is a companion sharing the
# same severity contract: WARN by default, refuse under ZS_FRESHNESS_STRICT=1,
# and return 2 for cannot-answer rather than passing quietly.
#
# The gap this closes: `tests/golden_path.sh` runs `pnpm build` inside an
# EXAMPLE, which consumes whatever `packages/vite-plugin/dist/` is already on disk.
# It never rebuilds the plugin, so a change to the plugin's own source can be
# silently untested -- the run is green about a dist that predates the edit.
#
# Usage:
#   zs_check_artifact_freshness "$ROOT" "packages/vite-plugin/dist/index.js" \
#     "packages/vite-plugin/src"
zs_check_artifact_freshness() {
  local root="$1" artifact="$2" srcdirs="$3"
  local strict="${ZS_FRESHNESS_STRICT:-0}"

  local existing=() missing=() d
  for d in $srcdirs; do
    if [ -d "$root/$d" ]; then existing+=("$root/$d"); else missing+=("$d"); fi
  done
  if [ "${#existing[@]}" -eq 0 ]; then
    echo "  FRESHNESS: none of the named source directories exist: $srcdirs" >&2
    echo "             The check cannot run, which is not the same as passing." >&2
    return 2
  fi
  # A PARTIALLY missing list is the dangerous case, and until 2026-08-27 it was
  # accepted silently: only the all-missing arm above refused, so a list whose
  # paths had been renamed out from under it kept scanning whatever survived and
  # printed exactly what a healthy run prints.
  #
  # That was live. The 2026-08-26 crate-directory rename (crates/plugin-db ->
  # crates/zeroship-data-v8, and seven siblings) left e2e_dev_vs_deployed_db.sh
  # naming eight directories of which only TWO still existed. The guard whose
  # whole job is "did you rebuild after touching plugin-db" had stopped looking
  # at plugin-db, and reported fresh either way.
  #
  # Refusing rather than warning, because this function exists to make staleness
  # loud, and a guard quietly covering a quarter of its subject IS the failure it
  # was written to prevent.
  if [ "${#missing[@]}" -gt 0 ]; then
    local _total=$(( ${#existing[@]} + ${#missing[@]} ))
    echo "  FRESHNESS: ${#missing[@]} of $_total named source directories do not exist:" >&2
    printf '               %s\n' "${missing[@]}" >&2
    echo "             Scanning only the survivors would report FRESH for a change" >&2
    echo "             under any missing path. Fix the caller's list." >&2
    return 2
  fi

  if [ ! -e "$root/$artifact" ]; then
    echo "  FRESHNESS: missing $artifact -- run pnpm build" >&2
    echo "             The check cannot run, which is not the same as passing." >&2
    return 2
  fi

  local newest_src
  newest_src=$(find "${existing[@]}" \( -name '*.ts' -o -name '*.tsx' \) \
    -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -1 | cut -d' ' -f2-)
  if [ -z "$newest_src" ]; then
    echo "  FRESHNESS: matched no .ts or .tsx files under: $srcdirs" >&2
    echo "             The check cannot run, which is not the same as passing." >&2
    return 2
  fi

  if [ "$newest_src" -nt "$root/$artifact" ]; then
    echo "  WARN $artifact is OLDER than $(printf '%s' "${newest_src#"$root"/}" | cut -d/ -f1-2)"
    echo "       ($(basename "$newest_src")). This run tests the dist on disk, not"
    echo "       your source change. Rebuild it, or a green result says nothing"
    echo "       about the edit you are trying to verify."
    [ "$strict" = "1" ] && return 1
    return 0
  fi

  echo "  ok   $artifact is newer than its source"
  return 0
}
