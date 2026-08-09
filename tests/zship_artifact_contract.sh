#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# A built .zship must match the artifact contract it declares about itself.
#
# `.zship` is a wire format (docs/reference/zship.md, crates/bundle): a
# zstd-compressed tar holding one `manifest.json` plus content-addressed
# `blobs/<sha256>`. The manifest then makes claims ABOUT those blobs -- each
# asset's `hash` and `size`, each brotli `variants.br`, and every entry in
# `worker.modules`.
#
# Nothing checked those claims against a real artifact. The e2e scripts read the
# manifest (the e2e_dev_vs_deployed_* family greps rpc auth postures out of it)
# but none opens the archive, so "the blob named <h> actually hashes to <h>"
# and "the recorded size is the real size" were unverified end to end. A blob
# whose name disagreed with its content would defeat the deploy path's
# content-addressed dedup silently: the store would treat two different bodies
# as the same object.
#
# This unpacks each artifact and checks, per file:
#   1. it is really zstd+tar, with exactly one manifest.json at the root
#   2. every non-manifest entry lives under blobs/ and is named [0-9a-f]{64}
#   3. no entry escapes the archive root (`..` or absolute paths)
#   4. every blob's NAME equals the sha256 of its own CONTENT
#   5. every manifest-referenced hash resolves to a blob that is present
#   6. every recorded `size` equals the blob's real byte length
#   7. worker.entry names a key that exists in worker.modules
#
# Orphan blobs (present but unreferenced) are REPORTED, not failed: an asset
# pipeline is entitled to carry a blob the manifest does not name today, and
# failing on it would encode an assumption this repo has not made.
#
# Runs over every examples/*/dist/app.zship it finds, so artifacts built by
# older plugin versions are covered too -- drift across builds is exactly what
# a single fresh artifact cannot show.
#
# Prereqs: zstd, tar, jq, sha256sum. No cargo, no database, no network.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0; ORPHANS=0; FILES=0
ok() { PASS=$((PASS+1)); }
no() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

# Corrupt one blob's content after unpacking, so its name no longer matches its
# hash. Proves check 4 is load-bearing rather than vacuously green.
MUTATE_CORRUPT_BLOB="${MUTATE_CORRUPT_BLOB:-0}"
# Rewrite a recorded size, proving check 6 is load-bearing independently of 4.
MUTATE_WRONG_SIZE="${MUTATE_WRONG_SIZE:-0}"

for t in zstd tar jq sha256sum; do
  command -v "$t" >/dev/null || { echo "FATAL: $t not on PATH"; exit 1; }
done

mapfile -t ARTIFACTS < <(find "$ROOT/examples" -path '*/dist/app.zship' -type f | sort)
if [ "${#ARTIFACTS[@]}" -eq 0 ]; then
  echo "FATAL: found no examples/*/dist/app.zship. Nothing was checked, which is"
  echo "       not the same as everything passing. Build at least one example."
  exit 1
fi

echo "=== .zship artifact contract: ${#ARTIFACTS[@]} artifacts ==="
[ "$MUTATE_CORRUPT_BLOB" = "1" ] && echo "  (MUTATION: corrupting one blob per artifact; check 4 must FAIL)"
[ "$MUTATE_WRONG_SIZE" = "1" ] && echo "  (MUTATION: falsifying one recorded size; check 6 must FAIL)"

for art in "${ARTIFACTS[@]}"; do
  name="$(basename "$(dirname "$(dirname "$art")")")"
  FILES=$((FILES+1))
  d="$WORK/$name"; mkdir -p "$d"

  if ! zstd -dc "$art" 2>/dev/null | tar -xf - -C "$d" 2>/dev/null; then
    no "$name: not a readable zstd+tar archive"
    continue
  fi

  # (1) exactly one manifest.json at the root
  [ -f "$d/manifest.json" ] || { no "$name: no manifest.json at archive root"; continue; }
  jq -e . "$d/manifest.json" >/dev/null 2>&1 || { no "$name: manifest.json is not valid JSON"; continue; }

  # (2)+(3) entry shape and no escapes. Checked on the ARCHIVE listing, not the
  # unpacked tree: tar would already have resolved a traversal on extract, so
  # asking the extracted directory could not see it.
  entries=$(zstd -dc "$art" 2>/dev/null | tar -tf - 2>/dev/null)
  bad=$(printf '%s\n' "$entries" | grep -vE '^(manifest\.json|blobs/[0-9a-f]{64})$' || true)
  if [ -n "$bad" ]; then
    no "$name: entries outside the contract:"
    printf '%s\n' "$bad" | head -5 | sed 's/^/        /'
  else
    ok
  fi

  if [ "$MUTATE_CORRUPT_BLOB" = "1" ]; then
    victim=$(find "$d/blobs" -type f | sort | head -1)
    [ -n "$victim" ] && printf 'x' >> "$victim"
  fi

  # (4) content-addressing actually holds
  mismatched=0; checked=0
  while IFS= read -r blob; do
    [ -n "$blob" ] || continue
    checked=$((checked+1))
    want="$(basename "$blob")"
    got="$(sha256sum "$blob" | cut -d' ' -f1)"
    if [ "$want" != "$got" ]; then
      mismatched=$((mismatched+1))
      [ "$mismatched" -le 3 ] && echo "        blob named $want hashes to $got"
    fi
  done < <(find "$d/blobs" -type f 2>/dev/null | sort)

  if [ "$checked" -eq 0 ]; then
    no "$name: archive carries zero blobs - nothing to content-address"
  elif [ "$mismatched" -gt 0 ]; then
    no "$name: $mismatched of $checked blobs do not hash to their own name"
  else
    ok
  fi

  # Referenced (path, hash, size) triples, extracted GENERICALLY: every 64-hex
  # string anywhere in the manifest counts as a blob reference, and a size is
  # paired when the hash's own object carries one.
  #
  # An earlier version enumerated the shapes it knew -- assets, variants.br,
  # worker.modules -- and consequently reported `runtime_descriptor.hash` as an
  # unreferenced orphan in two artifacts. That was the checker being incomplete,
  # not the artifacts being wrong, and it is exactly the failure this file exists
  # to catch, so the extractor now discovers references instead of listing them.
  jq -r '
    ([ paths(type=="object") as $p | getpath($p) as $o
       | select((($o.hash? // "") | tostring) | test("^[0-9a-f]{64}$"))
       | select($o.size? != null)
       | { key: $o.hash, value: ($o.size | tostring) } ] | from_entries) as $sz
    | [ paths(type=="string") as $p | getpath($p) as $v
        | select($v | test("^[0-9a-f]{64}$"))
        | (($p | join(".")) + "\t" + $v + "\t" + ($sz[$v] // "-")) ]
    | unique | .[]' "$d/manifest.json" 2>/dev/null > "$WORK/$name.refs"

  if [ "$MUTATE_WRONG_SIZE" = "1" ]; then
    awk 'NR==1 && $NF ~ /^[0-9]+$/ {$NF = $NF + 1} {print}' OFS='\t' "$WORK/$name.refs" > "$WORK/$name.refs.m" \
      && mv "$WORK/$name.refs.m" "$WORK/$name.refs"
  fi

  nrefs=$(grep -c . "$WORK/$name.refs" || true)
  if [ "$nrefs" -eq 0 ]; then
    no "$name: manifest references no blobs at all - the extractor found nothing to check"
    continue
  fi

  # (5) referenced blobs are present, and (6) recorded sizes are real
  missing=0; wrongsize=0
  while IFS=$'\t' read -r what hash size; do
    [ -n "$hash" ] || continue
    f="$d/blobs/$hash"
    if [ ! -f "$f" ]; then
      missing=$((missing+1))
      [ "$missing" -le 3 ] && echo "        $what references absent blob $hash"
      continue
    fi
    if [ "$size" != "-" ]; then
      real=$(stat -c%s "$f")
      if [ "$real" != "$size" ]; then
        wrongsize=$((wrongsize+1))
        [ "$wrongsize" -le 3 ] && echo "        $what records size=$size but blob is $real bytes"
      fi
    fi
  done < "$WORK/$name.refs"

  [ "$missing" -eq 0 ] && ok || no "$name: $missing of $nrefs referenced blobs are absent from the archive"
  [ "$wrongsize" -eq 0 ] && ok || no "$name: $wrongsize recorded sizes disagree with the blob"

  # (7) the entry module is one of the modules -- but only for apps that HAVE a
  # worker. `examples/ssg-docs` is a purely static site: `.worker` is null and
  # every resource is a `static` rule, so it has no entry to name and demanding
  # one was this checker's error, not the artifact's.
  if [ "$(jq -r '.worker | type' "$d/manifest.json")" = "null" ]; then
    static_only=$(jq -r '[.resources // {} | to_entries[] | select(.value.static | not)] | length' "$d/manifest.json")
    if [ "$static_only" = "0" ]; then
      ok
    else
      no "$name: no worker section, yet $static_only resource(s) are not static rules"
    fi
  else
    entry=$(jq -r '.worker.entry // empty' "$d/manifest.json")
    if [ -z "$entry" ]; then
      no "$name: has a worker section but declares no worker.entry"
    elif jq -e --arg e "$entry" '.worker.modules | has($e)' "$d/manifest.json" >/dev/null 2>&1; then
      ok
    else
      no "$name: worker.entry '$entry' is not a key in worker.modules"
    fi
  fi

  # Orphans: reported, never failed. See the header.
  refd=$(cut -f2 "$WORK/$name.refs" | sort -u)
  present=$(find "$d/blobs" -type f -printf '%f\n' 2>/dev/null | sort -u)
  orph=$(comm -13 <(printf '%s\n' "$refd") <(printf '%s\n' "$present") | grep -c . || true)
  if [ "$orph" -gt 0 ]; then
    ORPHANS=$((ORPHANS+orph))
    echo "  note $name: $orph blob(s) present but unreferenced (reported, not a failure)"
  fi
done

echo ""
echo "  artifacts: $FILES    checks passed: $PASS    failed: $FAIL    unreferenced blobs noted: $ORPHANS"
echo "  MUTATIONS: MUTATE_CORRUPT_BLOB=1 must fail check 4; MUTATE_WRONG_SIZE=1 must fail check 6"
[ "$FAIL" -eq 0 ]
