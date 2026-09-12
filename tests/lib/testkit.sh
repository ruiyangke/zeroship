# shellcheck shell=bash
# Resolve the existing xtask package's database fixture binary through Cargo.
zs_testkit_resolve_bin() {
  local root
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  ZS_TESTKIT_BIN="$root/xtask/target/debug/zs-testkit"
  [ "${ZS_TESTKIT_CHECKED:-}" = "$ZS_TESTKIT_BIN" ] && return 0
  if ! cargo build -q --manifest-path "$root/xtask/Cargo.toml" --bin zs-testkit --target-dir "$root/xtask/target" >&2; then
    echo "FATAL: could not build the database fixture command in xtask." >&2
    return 2
  fi
  ZS_TESTKIT_CHECKED="$ZS_TESTKIT_BIN"
}
zs_testkit() {
  zs_testkit_resolve_bin || return $?
  "$ZS_TESTKIT_BIN" "$@"
}
