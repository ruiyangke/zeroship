#!/usr/bin/env bash
# Protect responsibility boundaries inside the consolidated data crates.
set -euo pipefail
cd "$(dirname "$0")/.."
. tests/lib/gate_arms.sh
gate_arms_init data_boundary

failed=0
contract_files=(crates/zeroship-data-orm/src/{driver,executor,protection,search,storage,cdc/source}.rs)
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

adapter_dependency_is_forbidden() {
  case "$1" in
    aes-gcm|hkdf|hmac|zeroize|rusqlite|sqlite-vec|compio-postgres|zeroship-migrate-policy) return 0 ;;
    *) return 1 ;;
  esac
}

# Driver construction and protection policy implementation belong to the ORM.
# Adapter fixtures may still use them through dev-dependencies.
adapter=$(cargo metadata --no-deps --format-version 1 | jq -ec '
  [.packages[] | select(.name == "zeroship-data-v8")] |
  if length == 1 then .[0] else error("adapter package missing or repeated") end')
dependencies=$(jq -r '.dependencies[] | select(.kind != "dev") | .name' <<<"$adapter")
checked=0
while IFS= read -r dependency; do
  [[ -n "$dependency" ]] || continue
  checked=$((checked + 1))
  if adapter_dependency_is_forbidden "$dependency"; then
    echo "FAIL: ORM implementation dependency in V8 adapter: $dependency"
    failed=1
  fi
done <<<"$dependencies"
gate_arm adapter_dependencies "$checked" 1

adapter_exports_owner() {
  rg -n -U 'pub[[:space:]]+use[[:space:]]+(zeroship_data_(orm|sql)|backend)(::|[[:space:];])'
}

checked=0
while IFS= read -r file; do
  checked=$((checked + 1))
  if adapter_exports_owner < "$file"; then
    echo "FAIL: ORM/SQL re-export in V8 adapter: $file"
    failed=1
  fi
done < <(rg --files crates/zeroship-data-v8/src -g '*.rs')
gate_arm adapter_exports "$checked" 1

# Every source file participates, including adapter integration helpers.
adapter_names_backend() {
  rg -n '\b(compio_postgres|rusqlite|PostgresBackend|SqliteBackend|BackendUrl)\b'
}
checked=0
while IFS= read -r file; do
  checked=$((checked + 1))
  if adapter_names_backend < "$file"; then
    echo "FAIL: concrete backend knowledge in V8 adapter: $file"
    failed=1
  fi
done < <(rg --files crates/zeroship-data-v8/src -g '*.rs')
gate_arm adapter_backend_knowledge "$checked" 1

checked=0
for dependency in aes-gcm hkdf hmac zeroize rusqlite sqlite-vec compio-postgres zeroship-migrate-policy; do
  checked=$((checked + 1))
  adapter_dependency_is_forbidden "$dependency" || failed=1
done
for dependency in zeroship-data-orm zeroship-data-sql zeroship-runtime; do
  checked=$((checked + 1))
  if adapter_dependency_is_forbidden "$dependency"; then failed=1; fi
done
for declaration in 'pub use zeroship_data_orm::cdc::broker;' 'pub use zeroship_data_sql as sql;' $'pub\nuse\nbackend::pg_row_json;'; do
  checked=$((checked + 1))
  adapter_exports_owner <<<"$declaration" >/dev/null || failed=1
done
for declaration in 'use zeroship_data_orm::cdc::broker;' 'pub(crate) use zeroship_data_sql::compile;'; do
  checked=$((checked + 1))
  if adapter_exports_owner <<<"$declaration" >/dev/null; then failed=1; fi
done
for declaration in 'compio_postgres::Pool' 'PostgresBackend::connect(url)' 'BackendUrl::Sqlite' 'rusqlite::Connection' 'SqliteBackend::open'; do
  checked=$((checked + 1))
  adapter_names_backend <<<"$declaration" >/dev/null || failed=1
done
for declaration in 'ConnectionFactory::for_url(url)' 'backend.ensure(keys)' 'BackendHandle'; do
  checked=$((checked + 1))
  if adapter_names_backend <<<"$declaration" >/dev/null; then failed=1; fi
done
gate_arm adapter_controls "$checked" 1

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
