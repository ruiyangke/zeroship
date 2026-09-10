#!/usr/bin/env bash
# Protect responsibility boundaries inside the consolidated data crates.
set -euo pipefail
cd "$(dirname "$0")/.."
. tests/lib/gate_arms.sh
gate_arms_init data_boundary

failed=0
contract_files=(crates/zeroship-data-orm/src/{driver,executor,protection,search,storage}.rs)
checked=0
for file in "${contract_files[@]}"; do
  checked=$((checked + 1))
  if rg -n '^pub trait (SqlExecutor|SchemaIntrospect|VectorIndex|SpatialIndex|DialectBuilder)\b' "$file"; then
    echo "FAIL: duplicate runtime contract in $file"
    failed=1
  fi
done
gate_arm authoritative_contracts "$checked" 5

checked=0
for file in crates/zeroship-data-orm/src/crud/{mod,write_pipeline,read_pipeline}.rs; do
  checked=$((checked + 1))
  production=$(sed '/^#\[cfg(test)\]/,$d' "$file")
  if printf '%s\n' "$production" | rg -n '^[[:space:]]*(if|match).*SqlDialect::|^fn (lower_boolean|encode_sqlite|normalize_(boolean|timestamp))'; then
    echo "FAIL: physical dialect conversion in $file"
    failed=1
  fi
done
gate_arm dialect_codecs "$checked" 3

checked=0
for file in crates/zeroship-data-orm/src/{schema_cache,tx_lanes,protection/mask_policy,protection/protection_floor}.rs; do
  checked=$((checked + 1))
  if rg -n '^thread_local!' "$file"; then
    echo "FAIL: state outside OrmContext in $file"
    failed=1
  fi
done
gate_arm context_ownership "$checked" 4

# Negative controls prove the same predicates recognize a forbidden source shape.
checked=0
for declaration in SqlExecutor SchemaIntrospect VectorIndex SpatialIndex DialectBuilder; do
  checked=$((checked + 1))
  printf 'pub trait %s {}\n' "$declaration" | rg -q '^pub trait (SqlExecutor|SchemaIntrospect|VectorIndex|SpatialIndex|DialectBuilder)\b' || failed=1
done
checked=$((checked + 1))
printf 'if dialect == SqlDialect::Sqlite {\n' | rg -q '^[[:space:]]*(if|match).*SqlDialect::|^fn (lower_boolean|encode_sqlite|normalize_(boolean|timestamp))' || failed=1
checked=$((checked + 1))
printf 'thread_local! {\n' | rg -q '^thread_local!' || failed=1
gate_arm negative_controls "$checked" 7

gate_arms_finish || failed=1
exit "$failed"
