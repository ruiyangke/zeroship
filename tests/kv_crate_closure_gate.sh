#!/usr/bin/env bash
# Storage and its tests must remain independent of V8 and platform metering.
set -euo pipefail
cd "$(dirname "$0")/.."
source tests/lib/gate_arms.sh
gate_arms_init kv_crate_closure

kv_tree=$(cargo tree -p zeroship-kv --all-features -e normal,build,dev --prefix none --format '{p}')
kv_packages=$(printf '%s\n' "$kv_tree" | awk '{print $1}' | sort -u)
examined=0
failed=0
while IFS= read -r package; do
  [ -n "$package" ] || continue
  examined=$((examined + 1))
  case "$package" in
    v8|zeroship-runtime|zeroship-runtime-macros|zeroship-*-v8|zeroship-metering|zeroship-plugin-*)
      echo "FAIL: zeroship-kv reaches $package" >&2
      failed=1
      ;;
  esac
done <<< "$kv_packages"
gate_arm storage_dependencies "$examined" 20

# The binding is a live positive control for the dependency enumeration.
binding_tree=$(cargo tree -p zeroship-kv-v8 -e normal,build --prefix none --format '{p}')
binding_packages=$(printf '%s\n' "$binding_tree" | awk '{print $1}' | sort -u)
examined=0
for package in zeroship-kv zeroship-runtime v8; do
  examined=$((examined + 1))
  if ! grep -Fxq "$package" <<< "$binding_packages"; then
    echo "FAIL: binding does not reach required dependency $package" >&2
    failed=1
  fi
done
gate_arm binding_dependencies "$examined" 1
gate_arms_finish
[ "$failed" -eq 0 ]
echo 'KV storage and V8 binding dependency boundaries hold.'
