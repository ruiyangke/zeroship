#!/usr/bin/env bash
# Full-text search was removed from the migration engine. Keep every live
# platform layer and current contract document from advertising or rebuilding
# the deleted feature.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init db_fts_deletion

FAILURES=0
fail() {
  FAILURES=$((FAILURES + 1))
  echo "  FAIL $1" >&2
}

CODE_PATTERN='(^|[^[:alnum:]])fts(5)?([^[:alnum:]]|$)|ftsLanguage|tsvector|fulltextindex|full[-_]text|(^|[^[:alnum:]])_rank\??[[:space:]]*:|\$search'
SOURCE_NAMES=(
  -name '*.rs' -o -name '*.ts' -o -name '*.tsx' -o -name '*.js' -o
  -name '*.jsx' -o -name '*.mjs' -o -name '*.cjs' -o -name '*.mts' -o
  -name '*.cts'
)

scan_code_arm() {
  local arm="$1"
  local floor="$2"
  shift 2
  local -a files=("$@")
  local findings status finding_count sample

  if ! gate_arm "$arm" "${#files[@]}" "$floor"; then
    FAILURES=$((FAILURES + 1))
    return
  fi

  findings="$(LC_ALL=C grep -HinEi -- "$CODE_PATTERN" "${files[@]}")"
  status=$?
  if [ "$status" -gt 1 ]; then
    fail "$arm scan failed with grep status $status"
  elif [ -n "$findings" ]; then
    finding_count="$(printf '%s\n' "$findings" | grep -c .)"
    sample="$(printf '%s\n' "$findings" | sed -n '1,12p')"
    fail "deleted database full-text search remains in $finding_count $arm line(s); first 12:"
    printf '%s\n' "$sample" >&2
  else
    echo "  ok   ${#files[@]} $arm file(s) contain no deleted FTS surface"
  fi
}

# The migration-engine Rust crates are intentionally outside these arms. They
# retain the removal record that explains why no producer exists. Each live
# source root has its own floor so losing SDK or example coverage cannot pass on
# the crate count alone.
mapfile -d '' -t CRATE_FILES < <(
  find crates -type f \( "${SOURCE_NAMES[@]}" \) \
    ! -path '*/node_modules/*' \
    ! -path '*/dist/*' \
    ! -path 'crates/zeroship-runtime/tests/fixtures/*' \
    ! -path 'crates/zeroship-migrate/*' \
    ! -path 'crates/zeroship-migrate-*/*' \
    -print0 | LC_ALL=C sort -z
)
scan_code_arm live_crate_code "900" "${CRATE_FILES[@]}"

# Migration-engine comments retain the deliberate removal record. Scan the
# executable/source lines, plus the generated JSON wire schema, so the deleted
# producer and its former trust flags cannot return behind that comment record.
mapfile -d '' -t MIGRATION_FILES < <(
  find crates -type f \
    \( \( "${SOURCE_NAMES[@]}" \) -o -name '*.json' \) \
    \( -path 'crates/zeroship-migrate/*' -o -path 'crates/zeroship-migrate-*/*' \) \
    ! -path '*/node_modules/*' \
    ! -path '*/dist/*' \
    -print0 | LC_ALL=C sort -z
)
MIGRATION_PATTERN="$CODE_PATTERN|engine_goodie_ddl|GENERATED_PREFIX|pragma_name\\.eq_ignore_ascii_case\\(\"data_version\"\\)"
if gate_arm migration_engine_code "${#MIGRATION_FILES[@]}" 500; then
  MIGRATION_RAW_FINDINGS="$(LC_ALL=C grep -HinEi -- "$MIGRATION_PATTERN" "${MIGRATION_FILES[@]}")"
  MIGRATION_STATUS=$?
  if [ "$MIGRATION_STATUS" -gt 1 ]; then
    fail "migration_engine_code scan failed with grep status $MIGRATION_STATUS"
  else
    MIGRATION_FINDINGS="$(
      printf '%s\n' "$MIGRATION_RAW_FINDINGS" \
        | LC_ALL=C grep -Ev '^[^:]+:[0-9]+:[[:space:]]*(//|/\*|\*)' \
        || true
    )"
  fi
  if [ "$MIGRATION_STATUS" -le 1 ] && [ -n "$MIGRATION_FINDINGS" ]; then
    MIGRATION_FINDING_COUNT="$(printf '%s\n' "$MIGRATION_FINDINGS" | grep -c .)"
    MIGRATION_SAMPLE="$(printf '%s\n' "$MIGRATION_FINDINGS" | sed -n '1,12p')"
    fail "deleted database full-text search remains in $MIGRATION_FINDING_COUNT migration_engine_code line(s); first 12:"
    printf '%s\n' "$MIGRATION_SAMPLE" >&2
  elif [ "$MIGRATION_STATUS" -le 1 ]; then
    echo "  ok   ${#MIGRATION_FILES[@]} migration_engine_code file(s) contain no deleted FTS implementation"
  fi
else
  FAILURES=$((FAILURES + 1))
fi

mapfile -d '' -t SDK_FILES < <(
  find sdks -type f \( "${SOURCE_NAMES[@]}" \) \
    ! -path '*/node_modules/*' \
    ! -path '*/dist/*' \
    -print0 | LC_ALL=C sort -z
)
scan_code_arm live_sdk_code "500" "${SDK_FILES[@]}"

mapfile -d '' -t PACKAGE_FILES < <(
  find packages/zero-migrate -type f \( \( "${SOURCE_NAMES[@]}" \) -o -name '*.json' \) \
    ! -path '*/node_modules/*' \
    ! -path '*/dist/*' \
    -print0 | LC_ALL=C sort -z
)
scan_code_arm live_migrate_package_code "30" "${PACKAGE_FILES[@]}"

mapfile -d '' -t EXAMPLE_FILES < <(
  find examples -type f \( "${SOURCE_NAMES[@]}" \) \
    ! -path '*/node_modules/*' \
    ! -path '*/dist/*' \
    -print0 | LC_ALL=C sort -z
)
scan_code_arm live_example_code "150" "${EXAMPLE_FILES[@]}"

mapfile -d '' -t DOC_FILES < <(
  {
    find docs/reference -type f -name '*.md' -print0
    printf '%s\0' docs/feature-map.md AGENTS.md
  } | LC_ALL=C sort -z
)

DOC_PATTERN='(^|[^[:alnum:]])fts(5)?([^[:alnum:]]|$)|ftsLanguage|tsvector|full[-_ ]text|(^|[^[:alnum:]])_rank([^[:alnum:]]|$)|\$search'
if gate_arm current_doc_files "${#DOC_FILES[@]}" 20; then
  DOC_FINDINGS="$(LC_ALL=C grep -HinEi -- "$DOC_PATTERN" "${DOC_FILES[@]}")"
  DOC_STATUS=$?
  if [ "$DOC_STATUS" -gt 1 ]; then
    fail "the current-document scan failed with grep status $DOC_STATUS"
  elif [ -n "$DOC_FINDINGS" ]; then
    DOC_FINDING_COUNT="$(printf '%s\n' "$DOC_FINDINGS" | grep -c .)"
    DOC_SAMPLE="$(printf '%s\n' "$DOC_FINDINGS" | sed -n '1,12p')"
    fail "deleted database full-text search remains in $DOC_FINDING_COUNT current-document line(s); first 12:"
    printf '%s\n' "$DOC_SAMPLE" >&2
  else
    echo "  ok   ${#DOC_FILES[@]} current document(s) contain no deleted FTS surface"
  fi
else
  FAILURES=$((FAILURES + 1))
fi

gate_arms_finish || FAILURES=$((FAILURES + 1))

if [ "$FAILURES" -ne 0 ]; then
  echo "db FTS deletion gate: $FAILURES failure(s)" >&2
  exit 1
fi

echo "db FTS deletion gate: PASS"
