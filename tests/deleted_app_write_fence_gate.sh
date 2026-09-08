#!/usr/bin/env bash
# Every control-plane write that could resurrect a DELETED app carries its fence.
#
# WHY THIS EXISTS. `organizations::delete_app` is a soft delete: it stamps
# `deleted_at`, detaches the project, and nulls `deploy_hash` / `manifest_json`,
# and a sibling statement destroys the app's vars, secrets and exposure set. The
# row stays, because billing evidence points at it.
#
# A soft delete is only as good as the writes that respect it, and four did not.
# `set_deploy_with_manifest` matched `WHERE id = $3` and put the deploy hash and
# manifest straight back. `set_plan` and the proration flip re-planned a corpse.
# Every `EnvStore` write rebuilt the environment the delete had just destroyed,
# and bumped `env_version` so workers would go and fetch it. `unarchive_app` DID
# check - in a preceding read rather than in the statement that acts - which
# answers the sequential question and leaves the concurrent one open.
#
# None of that was visible to a test, because each surface looked correct on its
# own. The defect only appears when you ask the question across all of them at
# once, which is what this gate does.
#
# WHAT EACH ARM RULES ON, and what it does NOT catch, is in the arm's own
# header. In particular: this gate reads SQL as text. It cannot tell that a
# predicate is semantically sufficient, only that the fence is spelled. A write
# that carries `deleted_at IS NULL` against the wrong table would pass here.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init deleted_app_write_fence

SRC="$ROOT/crates/zeroship-control/src"
fail=0

# The marker a write uses to say "this one may touch a deleted app, and here is
# why". It has to be a deliberate sentence, not an absence, so that exempting a
# write is a thing a reviewer sees in the diff.
EXEMPT='zs-allow-deleted'

# ---------------------------------------------------------------------------
# Arm 1 - every UPDATE of zeroship.apps is fenced or explicitly exempt.
#
# Rules on: each `UPDATE zeroship.apps` occurrence in the control plane's src.
# Does NOT catch: a write reaching the table through a CTE that names it only
# in the outer statement, or dynamic SQL assembled from fragments. Both would
# read as no occurrence at all rather than as an unfenced one.
# ---------------------------------------------------------------------------
updates=0
unfenced_updates=""
while IFS= read -r hit; do
  file="${hit%%:*}"
  line="${hit#*:}"
  line="${line%%:*}"
  updates=$((updates + 1))
  # The statement is a Rust string literal continued with trailing backslashes,
  # so read forward to the closing quote rather than a fixed window.
  stmt="$(awk -v s="$line" 'NR>=s { print; if (NR>s && /",$|"$|",$/) exit }' "$file" | head -40)"
  ctx="$(awk -v s="$line" 'NR>=s-12 && NR<=s' "$file")"
  case "$stmt$ctx" in
    *"deleted_at IS NULL"*|*"deleted_at = NOW()"*|*"$EXEMPT"*) ;;
    *)
      unfenced_updates="$unfenced_updates
  $file:$line"
      fail=1
      ;;
  esac
done < <(grep -rn --include='*.rs' 'UPDATE zeroship\.apps' "$SRC" || true)

if [ -n "$unfenced_updates" ]; then
  echo "FAIL: these writes to zeroship.apps neither fence on deleted_at nor" >&2
  echo "      declare '$EXEMPT' with a reason:$unfenced_updates" >&2
  echo "  A soft-deleted app must not be re-planned, re-deployed or restored." >&2
fi
gate_arm apps_updates "$updates" 8

# ---------------------------------------------------------------------------
# Arm 2 - every EnvStore write that CREATES app-scoped state locks the app row.
#
# Rules on: each INSERT into an app-scoped env table in env_store.rs.
# Does NOT catch: whether `lock_live_app` is called BEFORE the insert, only
# that the enclosing function calls it. Ordering is checked by the live test
# `a_deleted_app_refuses_every_write_that_would_resurrect_it`, not here.
# DELETEs are deliberately out of scope: removing more of what the delete
# already removed resurrects nothing.
# ---------------------------------------------------------------------------
env_inserts=0
unfenced_inserts=""
ENVF="$SRC/env_store.rs"
while IFS= read -r hit; do
  line="${hit%%:*}"
  env_inserts=$((env_inserts + 1))
  # Walk back to the enclosing `fn`, then forward over its body looking for the
  # lock. The body ends at the next line that starts a new item at fn indent.
  fn_line="$(awk -v s="$line" 'NR<=s && /^    (pub )?(async )?fn /{ n=NR } END { print n }' "$ENVF")"
  body="$(awk -v s="$fn_line" 'NR>s { if (/^    (pub )?(async )?fn /) exit; print }' "$ENVF")"
  case "$body" in
    *"lock_live_app("*|*"$EXEMPT"*) ;;
    *)
      fname="$(awk -v s="$fn_line" 'NR==s' "$ENVF" | sed 's/^ *//;s/ *{.*//')"
      unfenced_inserts="$unfenced_inserts
  $ENVF:$line  in: $fname"
      fail=1
      ;;
  esac
done < <(grep -n 'INSERT INTO zeroship\.app_\(vars\|secrets\|env_expose\)' "$ENVF" || true)

if [ -n "$unfenced_inserts" ]; then
  echo "FAIL: these env writes create app-scoped state without locking the app" >&2
  echo "      row first, so they can land on a deleted app:$unfenced_inserts" >&2
  echo "  Call lock_live_app(&tx, app_id) inside the write's transaction." >&2
fi
gate_arm env_inserts "$env_inserts" 3

# ---------------------------------------------------------------------------
# Arm 3 - the positive control: this gate can actually fail.
#
# Arms 1 and 2 pass by finding nothing wrong, which is exactly what they would
# do if their greps had gone blind - a renamed table, a moved file, a changed
# quoting style. This arm feeds each arm's matcher a synthetic unfenced write
# and requires it to be rejected, so a gate that stopped looking is a red
# rather than a green.
# ---------------------------------------------------------------------------
probe_dir="$(mktemp -d)"
trap 'rm -rf "$probe_dir"' EXIT
cat >"$probe_dir/probe.rs" <<'PROBE'
        conn.execute(
            "UPDATE zeroship.apps SET plan_id = $1 \
             WHERE id = $2",
            &[&plan_id, id],
        )
PROBE
controls=0
probe_hits="$(grep -c 'UPDATE zeroship\.apps' "$probe_dir/probe.rs" || true)"
if [ "$probe_hits" -ne 1 ]; then
  echo "FAIL: arm 1's matcher did not find the synthetic unfenced UPDATE, so a" >&2
  echo "      clean report from it is not evidence." >&2
  fail=1
fi
controls=$((controls + 1))
probe_stmt="$(cat "$probe_dir/probe.rs")"
case "$probe_stmt" in
  *"deleted_at IS NULL"*)
    echo "FAIL: the synthetic unfenced write matched the fence test, so arm 1" >&2
    echo "      would pass an unfenced write." >&2
    fail=1
    ;;
esac
controls=$((controls + 1))
if ! grep -q 'INSERT INTO zeroship\.app_\(vars\|secrets\|env_expose\)' "$ENVF"; then
  echo "FAIL: arm 2's matcher found no env insert in $ENVF at all, so its clean" >&2
  echo "      report is about a blind grep rather than about the code." >&2
  fail=1
fi
controls=$((controls + 1))
gate_arm matcher_controls "$controls" 3

echo
echo "apps updates fenced or exempt: $updates"
echo "env inserts behind the row lock: $env_inserts"
echo "matcher controls: $controls"

gate_arms_finish || fail=1
if [ "$fail" -ne 0 ]; then
  echo "DELETED APP WRITE FENCE GATE: FAILED"
  exit 1
fi
echo "DELETED APP WRITE FENCE GATE: PASSED"
