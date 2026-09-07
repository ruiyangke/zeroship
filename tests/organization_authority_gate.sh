#!/usr/bin/env bash
# ============================================================================
# organization_authority_gate.sh - prove on a real PostgreSQL that the
# organization model's authority claims are UNSPELLABLE, not merely unwritten.
#
# WHY THIS EXISTS. The migration that introduces organizations, projects and
# their membership edges argues, in prose, that four things cannot be said:
#
#   - a project membership naming a project in another organization
#   - a project membership naming a user who is not a member of that
#     organization
#   - a role that is not on the closed ladder
#   - an invite granting more authority than its issuer held
#
# Prose does not re-measure itself. Every one of those is enforced by a
# constraint the database evaluates, so every one of them can be ASKED. This
# gate asks, against the committed corpus applied to an empty database.
#
# WHY EACH CASE NAMES THE CONSTRAINT IT EXPECTS. A refusal is not evidence on
# its own. While this gate was being written, the case "an app naming a project
# that does not exist" was refused - by `apps_plan_fk`, because the fixture had
# no plan row, so the edge under test was never reached. A case that only
# checked "the statement failed" would have reported a green for a constraint
# it never exercised. Each case therefore records the constraint (or, for a
# NOT NULL violation, the column) PostgreSQL names in its own diagnostics, and
# the expectation table below pins it.
#
# EVERY REFUSAL IS PAIRED WITH A CONTROL that differs in one variable and MUST
# be accepted. Without them, a fixture that failed to insert anything at all
# would refuse every case and read as a perfect pass.
#
# WHAT THIS GATE DOES NOT RULE ON. The per-project narrowing rule itself -
# effective rank is min(organization rank, project rank) - is computed by the
# authorization vocabulary at read time and is not expressible as a constraint
# (a CHECK cannot subquery, and freezing the organization rank into the project
# row would make a demotion FAIL rather than narrow). This gate proves the
# schema the rule stands on, never the rule.
#
# Usage:
#   tests/organization_authority_gate.sh
#   tests/organization_authority_gate.sh --dsn <url>   # an EMPTY database
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

DSN=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dsn) DSN="${2:-}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

gate_arms_init organization_authority

OWN_CONTAINER=""
WORK_DIR="$(mktemp -d -t zeroship-organization-authority.XXXXXX)"
cleanup() {
  if [ -n "$OWN_CONTAINER" ]; then
    docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  fi
  [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ] && rm -rf -- "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

echo "== organization authority gate =="

# A port range of this gate's own. The corpus gate takes 5560-5599; two gates
# racing for one port is a failure that looks like a database fault.
if [ -z "$DSN" ]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "FAIL: no --dsn given and docker is not on PATH" >&2
    exit 2
  fi
  port=""
  for p in $(seq 5600 5639); do
    if ! ss -ltn 2>/dev/null | grep -q ":$p "; then
      port="$p"
      break
    fi
  done
  if [ -z "$port" ]; then
    echo "FAIL: no free TCP port in 5600-5639 for PostgreSQL" >&2
    exit 2
  fi
  OWN_CONTAINER="zs-orgauth-gate-$port"
  docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  echo "starting fresh PostgreSQL on 127.0.0.1:$port ($OWN_CONTAINER)"
  if ! docker run -d --name "$OWN_CONTAINER" \
      -p "127.0.0.1:$port:5432" \
      -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
      postgres:17 >/dev/null 2>&1; then
    echo "FAIL: could not start PostgreSQL container" >&2
    exit 2
  fi
  if ! docker exec "$OWN_CONTAINER" sh -c \
      'for i in $(seq 1 60); do pg_isready -U postgres -q && exit 0; sleep 1; done; exit 1'; then
    echo "FAIL: PostgreSQL did not become ready" >&2
    exit 2
  fi
  DSN="postgres://postgres:zeroship@localhost:$port/zeroship"
fi

echo "applying the committed corpus with deploy/ops/db-migrate.sh"
if ! ZEROSHIP_MIGRATE_DSN="$DSN" bash "$ROOT/deploy/ops/db-migrate.sh" >"$WORK_DIR/apply.log" 2>&1; then
  echo "FAIL: the corpus did not apply; this gate has nothing to rule on" >&2
  tail -30 "$WORK_DIR/apply.log" >&2
  exit 2
fi

# The probe. Each case runs inside its own subtransaction and records what
# PostgreSQL decided, so an accepted case and a refused case are the SAME kind
# of row and the shell compares data rather than parsing psql's prose.
#
# `witness` is the constraint PostgreSQL names, or `column:<name>` for a NOT
# NULL violation, which carries a column rather than a constraint.
cat >"$WORK_DIR/probe.sql" <<'SQL'
\set ON_ERROR_STOP 1
\pset pager off
\pset tuples_only on
\pset format unaligned
\pset fieldsep '|'

DROP TABLE IF EXISTS probe_verdict;
DROP FUNCTION IF EXISTS probe(text, text);
CREATE TABLE probe_verdict (case_name text primary key, outcome text, sqlstate text, witness text);
-- The probing role is `zeroship_control` for part of this script, and it owns
-- nothing here. Without this the verdict INSERT would itself be refused, and
-- the refusal would land inside the handler that is trying to record one.
GRANT INSERT, SELECT ON probe_verdict TO PUBLIC;

CREATE FUNCTION probe(case_name text, stmt text) RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
  constraint_name text;
  column_name text;
BEGIN
  BEGIN
    EXECUTE stmt;
    INSERT INTO probe_verdict VALUES (case_name, 'accepted', '00000', '');
  EXCEPTION WHEN others THEN
    GET STACKED DIAGNOSTICS
      constraint_name = CONSTRAINT_NAME,
      column_name = COLUMN_NAME;
    INSERT INTO probe_verdict VALUES (
      case_name, 'refused', SQLSTATE,
      coalesce(nullif(constraint_name, ''), 'column:' || coalesce(column_name, '?')));
  END;
END;
$fn$;

-- Fixtures. Two users, two organizations, one project in each, and one
-- organization membership. The second user is deliberately a member of NOTHING.
INSERT INTO zeroship.users (id, email, name) VALUES
  ('11111111-1111-1111-1111-111111111111', 'member@gate.test', 'Member'),
  ('22222222-2222-2222-2222-222222222222', 'stranger@gate.test', 'Stranger');
INSERT INTO zeroship.plans (id, name, runtime_limits_json) VALUES ('free', 'Free', '{}')
  ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.organizations (id, slug, name, billing_email) VALUES
  ('org_0000000000000000000001', 'acme', 'Acme', 'billing@acme.test'),
  ('org_0000000000000000000002', 'other', 'Other', 'billing@other.test');
INSERT INTO zeroship.projects (id, organization_id, slug, name) VALUES
  ('prj_0000000000000000000001', 'org_0000000000000000000001', 'web', 'Web'),
  ('prj_0000000000000000000002', 'org_0000000000000000000002', 'web', 'Web');
INSERT INTO zeroship.organization_members (organization_id, user_id, role) VALUES
  ('org_0000000000000000000001', '11111111-1111-1111-1111-111111111111', 'admin');

-- ---- the narrowing, and the ownership edges that carry it -----------------
SELECT probe('project_member_in_its_own_organization', $$
  INSERT INTO zeroship.project_members (project_id, organization_id, user_id, role)
  VALUES ('prj_0000000000000000000001','org_0000000000000000000001',
          '11111111-1111-1111-1111-111111111111','developer') $$);
SELECT probe('project_member_naming_another_organizations_project', $$
  INSERT INTO zeroship.project_members (project_id, organization_id, user_id, role)
  VALUES ('prj_0000000000000000000002','org_0000000000000000000001',
          '11111111-1111-1111-1111-111111111111','developer') $$);
SELECT probe('project_member_naming_a_non_member', $$
  INSERT INTO zeroship.project_members (project_id, organization_id, user_id, role)
  VALUES ('prj_0000000000000000000001','org_0000000000000000000001',
          '22222222-2222-2222-2222-222222222222','developer') $$);
-- These name `id`, `name`, `project_id` and `organization_id` and NOTHING else.
-- `apps.api_key` was dropped by
-- db/migrations-ts/20260905000200_drop_app_api_key.ts, and a column list still
-- naming it does not fail in a way this gate could read: the statement is
-- refused at PARSE time with 42703, before any of the edges under test are
-- reached, and `GET STACKED DIAGNOSTICS` carries neither a constraint nor a
-- column, so every one of these would record `column:?`. That is the failure
-- this gate's header warns about - a refusal that is not evidence - and
-- `app_with_no_project` is where it bites hardest, because that case exists
-- precisely to name `column:project_id` as the witness.
--
-- `organization_id` IS NAMED, and its absence was exactly that failure in a
-- quieter form. It became NOT NULL in
-- db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts, so
-- omitting it refused `app_in_a_project` with `column:organization_id` -- and
-- the CASCADE from there is what makes this worth spelling out: with no app in
-- the project, `delete_a_project_that_still_owns_an_app` succeeded, so
-- `delete_an_organization_that_still_owns_a_project` succeeded too, so the
-- organization every later case names was GONE and each of them recorded a
-- foreign-key refusal against a row the fixture had already deleted. One
-- missing column produced seven failures, none of which was about the edge its
-- case existed to measure.
SELECT probe('app_in_a_project', $$
  INSERT INTO zeroship.apps (id, name, project_id, organization_id)
  VALUES (gen_random_uuid(), 'gate-app', 'prj_0000000000000000000001',
          'org_0000000000000000000001') $$);
SELECT probe('app_naming_an_absent_project', $$
  INSERT INTO zeroship.apps (id, name, project_id, organization_id)
  VALUES (gen_random_uuid(), 'gate-app-2', 'prj_0000000000000000000009',
          'org_0000000000000000000001') $$);
SELECT probe('app_with_no_project', $$
  INSERT INTO zeroship.apps (id, name, organization_id)
  VALUES (gen_random_uuid(), 'gate-app-3', 'org_0000000000000000000001') $$);
SELECT probe('delete_a_project_that_still_owns_an_app', $$
  DELETE FROM zeroship.projects WHERE id = 'prj_0000000000000000000001' $$);
SELECT probe('delete_an_organization_that_still_owns_a_project', $$
  DELETE FROM zeroship.organizations WHERE id = 'org_0000000000000000000001' $$);

-- ---- the closed ladder and escalation by deferred grant --------------------
SELECT probe('organization_member_with_a_role_off_the_ladder', $$
  INSERT INTO zeroship.organization_members (organization_id, user_id, role)
  VALUES ('org_0000000000000000000001','22222222-2222-2222-2222-222222222222','superuser') $$);
SELECT probe('organization_id_of_the_wrong_width', $$
  INSERT INTO zeroship.organizations (id, slug, name, billing_email)
  VALUES ('org_short','short','Short','x@gate.test') $$);
-- The issuer is an admin throughout: rank 30, billing_rank 10.
SELECT probe('admin_invites_a_developer', $$
  INSERT INTO zeroship.organization_invites
    (id, token_hash, organization_id, email, role, role_rank, role_billing_rank,
     invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at)
  VALUES ('ivt_0000000000000000000001','\x01','org_0000000000000000000001','invitee@gate.test',
          'developer',20,0,'11111111-1111-1111-1111-111111111111',30,10,
          'organization_invite', now() + interval '1 day') $$);
SELECT probe('admin_invites_an_owner', $$
  INSERT INTO zeroship.organization_invites
    (id, token_hash, organization_id, email, role, role_rank, role_billing_rank,
     invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at)
  VALUES ('ivt_0000000000000000000002','\x02','org_0000000000000000000001','owner@gate.test',
          'owner',40,20,'11111111-1111-1111-1111-111111111111',30,10,
          'organization_invite', now() + interval '1 day') $$);
-- The billing dimension on its own: `billing` ranks BELOW admin on authority
-- and ABOVE it on money, so a rank-only comparison would let this through.
SELECT probe('admin_invites_billing', $$
  INSERT INTO zeroship.organization_invites
    (id, token_hash, organization_id, email, role, role_rank, role_billing_rank,
     invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at)
  VALUES ('ivt_0000000000000000000003','\x03','org_0000000000000000000001','money@gate.test',
          'billing',10,20,'11111111-1111-1111-1111-111111111111',30,10,
          'organization_invite', now() + interval '1 day') $$);
SELECT probe('invite_freezing_a_rank_triple_off_the_ladder', $$
  INSERT INTO zeroship.organization_invites
    (id, token_hash, organization_id, email, role, role_rank, role_billing_rank,
     invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at)
  VALUES ('ivt_0000000000000000000004','\x04','org_0000000000000000000001','forged@gate.test',
          'developer',5,0,'11111111-1111-1111-1111-111111111111',30,10,
          'organization_invite', now() + interval '1 day') $$);
-- Same address in a different case: the uniqueness is over citext.
SELECT probe('a_second_live_invite_for_one_address', $$
  INSERT INTO zeroship.organization_invites
    (id, token_hash, organization_id, email, role, role_rank, role_billing_rank,
     invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at)
  VALUES ('ivt_0000000000000000000005','\x05','org_0000000000000000000001','INVITEE@GATE.TEST',
          'viewer',10,0,'11111111-1111-1111-1111-111111111111',30,10,
          'organization_invite', now() + interval '1 day') $$);

-- ---- what the control plane's own role may do -----------------------------
-- SET ROLE, not a new connection: privilege is checked against the current
-- role, and the platform roles carry no password to connect with. NOT `SET
-- LOCAL`, which is a no-op outside a transaction block and would leave every
-- case below running as the superuser that created the fixtures.
SET ROLE zeroship_control;
SELECT probe('control_reads_the_ladder', $$
  SELECT count(*) FROM zeroship.organization_roles $$);
SELECT probe('control_inserts_into_the_ladder', $$
  INSERT INTO zeroship.organization_roles (role, rank, billing_rank, label)
  VALUES ('superowner', 99, 99, 'forged') $$);
SELECT probe('control_updates_the_ladder', $$
  UPDATE zeroship.organization_roles SET rank = 99 WHERE role = 'viewer' $$);
SELECT probe('control_deletes_from_the_ladder', $$
  DELETE FROM zeroship.organization_roles WHERE role = 'viewer' $$);
SELECT probe('control_writes_a_membership', $$
  INSERT INTO zeroship.organization_members (organization_id, user_id, role)
  VALUES ('org_0000000000000000000001','22222222-2222-2222-2222-222222222222','viewer') $$);
RESET ROLE;

-- ---- removal is a DELETE, and it reaches the project rows ------------------
-- Not a refusal: a measured consequence. The organization membership deleted
-- here is the one the surviving project membership hangs from.
DELETE FROM zeroship.organization_members
 WHERE organization_id = 'org_0000000000000000000001'
   AND user_id = '11111111-1111-1111-1111-111111111111';
INSERT INTO probe_verdict
SELECT 'losing_organization_membership_drops_project_membership', 'accepted', '00000',
       'project_members_left:' || count(*)::text
FROM zeroship.project_members;

SELECT case_name, outcome, sqlstate, witness FROM probe_verdict ORDER BY case_name;
SQL

PSQL_IMAGE="postgres:17"
run_sql() { # <script-basename>
  if command -v psql >/dev/null 2>&1; then
    psql "$DSN" -q -v ON_ERROR_STOP=1 -f "$WORK_DIR/$1"
  else
    docker run --rm --network host -v "$WORK_DIR:/w" "$PSQL_IMAGE" \
      psql "$DSN" -q -v ON_ERROR_STOP=1 -f "/w/$1"
  fi
}
if ! run_sql probe.sql >"$WORK_DIR/verdicts.txt" 2>"$WORK_DIR/verdicts.err"; then
  echo "FAIL: the probe did not run to completion; no case has a verdict" >&2
  tail -30 "$WORK_DIR/verdicts.err" >&2
  exit 2
fi

# The expectation table: case | outcome | witness. An `accepted` case carries no
# witness. Membership of an arm is the first field of the triple's group.
expect_narrowing="\
project_member_in_its_own_organization|accepted|
project_member_naming_another_organizations_project|refused|project_members_project_ownership_fkey
project_member_naming_a_non_member|refused|project_members_organization_member_fkey
app_in_a_project|accepted|
app_naming_an_absent_project|refused|apps_project_ownership_fkey
app_with_no_project|refused|column:project_id
delete_a_project_that_still_owns_an_app|refused|apps_project_ownership_fkey
delete_an_organization_that_still_owns_a_project|refused|projects_organization_id_fkey
losing_organization_membership_drops_project_membership|accepted|project_members_left:0"

expect_escalation="\
organization_member_with_a_role_off_the_ladder|refused|organization_members_role_fkey
organization_id_of_the_wrong_width|refused|organizations_id_shape
admin_invites_a_developer|accepted|
admin_invites_an_owner|refused|organization_invites_no_escalation
admin_invites_billing|refused|organization_invites_no_escalation
invite_freezing_a_rank_triple_off_the_ladder|refused|organization_invites_role_fkey
a_second_live_invite_for_one_address|refused|organization_invites_one_active"

# A privilege refusal names no constraint, so these are pinned on SQLSTATE
# 42501 (insufficient_privilege) rather than on a witness.
expect_ladder="\
control_reads_the_ladder|accepted|00000
control_inserts_into_the_ladder|refused|42501
control_updates_the_ladder|refused|42501
control_deletes_from_the_ladder|refused|42501
control_writes_a_membership|accepted|00000"

verdict_field() { # <case> <field-number>
  awk -F'|' -v c="$1" -v f="$2" '$1 == c { print $(f); found = 1 } END { if (!found) print "<absent>" }' \
    "$WORK_DIR/verdicts.txt"
}

# The loop reads from a here-document, NOT a pipe, so it runs in THIS shell and
# its failure count survives. A `while | read` in a command substitution would
# increment BAD inside a subshell and lose every finding.
BAD=0
GROUP_N=0
check_group() { # <expectation-block> <compare-field: witness|sqlstate>
  local block="$1" mode="$2" case_name want_outcome want_value got_outcome got_value
  GROUP_N=0
  while IFS='|' read -r case_name want_outcome want_value; do
    [ -n "$case_name" ] || continue
    GROUP_N=$((GROUP_N + 1))
    got_outcome="$(verdict_field "$case_name" 2)"
    if [ "$mode" = sqlstate ]; then
      got_value="$(verdict_field "$case_name" 3)"
    else
      got_value="$(verdict_field "$case_name" 4)"
    fi
    if [ "$got_outcome" != "$want_outcome" ] || [ "$got_value" != "$want_value" ]; then
      echo "FAIL[$case_name]: expected $want_outcome/'$want_value', got $got_outcome/'$got_value'" >&2
      BAD=$((BAD + 1))
    fi
  done <<EOF
$block
EOF
}

check_group "$expect_narrowing" witness;  narrowing_n="$GROUP_N"
check_group "$expect_escalation" witness; escalation_n="$GROUP_N"
check_group "$expect_ladder" sqlstate;    ladder_n="$GROUP_N"

# Every case must have produced a row: a probe that silently recorded nothing
# would leave `<absent>` above, which the comparison catches, but a probe that
# recorded EXTRA cases means this table has stopped describing the script.
recorded="$(grep -c '|' "$WORK_DIR/verdicts.txt")"
expected_total=$((narrowing_n + escalation_n + ladder_n))
if [ "$recorded" -ne "$expected_total" ]; then
  echo "FAIL[coverage]: the probe recorded $recorded verdicts for $expected_total expectations" >&2
  BAD=$((BAD + 1))
fi

gate_arm narrowing "$narrowing_n" 6
gate_arm escalation "$escalation_n" 5
gate_arm ladder_authority "$ladder_n" 4

# ---- the collations, read back out of the catalog --------------------------
#
# A typed-id column and every foreign-key copy of one must be `text COLLATE
# "C"`; a missed copy is silent, degrading a join rather than erroring. The
# citext columns are in the same query on purpose: they are the control that
# proves this arm is reading real per-column state rather than a table default.
cat >"$WORK_DIR/collations.sql" <<'SQL'
\pset tuples_only on
\pset format unaligned
\pset fieldsep '|'
SELECT c.relname || '.' || a.attname || '|'
    || format_type(a.atttypid, a.atttypmod) || '|'
    || coalesce(co.collname, 'default')
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_collation co ON co.oid = a.attcollation
WHERE n.nspname = 'zeroship'
  AND a.attnum > 0 AND NOT a.attisdropped
  AND ((c.relname IN ('organizations','projects','organization_members',
                      'project_members','organization_invites')
        AND a.attname IN ('id','organization_id','project_id','slug','billing_email','email'))
    OR (c.relname = 'apps' AND a.attname = 'project_id'))
ORDER BY 1;
SQL
if ! run_sql collations.sql >"$WORK_DIR/collations.txt" 2>&1; then
  echo "FAIL[collation]: the catalog query did not run; this arm would rule on nothing" >&2
  tail -10 "$WORK_DIR/collations.txt" >&2
  exit 2
fi

expect_collations="\
apps.project_id|text|C
organization_invites.email|citext|default
organization_invites.id|text|C
organization_invites.organization_id|text|C
organization_members.organization_id|text|C
organizations.billing_email|citext|default
organizations.id|text|C
organizations.slug|citext|default
project_members.organization_id|text|C
project_members.project_id|text|C
projects.id|text|C
projects.organization_id|text|C
projects.slug|citext|default"

collation_n=0
while IFS= read -r want; do
  [ -n "$want" ] || continue
  collation_n=$((collation_n + 1))
  if ! grep -qxF "$want" "$WORK_DIR/collations.txt"; then
    got="$(grep -F "${want%%|*}|" "$WORK_DIR/collations.txt" || echo '<absent>')"
    echo "FAIL[collation]: expected '$want', got '$got'" >&2
    BAD=$((BAD + 1))
  fi
done <<EOF
$expect_collations
EOF

observed_collations="$(grep -c '|' "$WORK_DIR/collations.txt")"
if [ "$observed_collations" -ne "$collation_n" ]; then
  echo "FAIL[collation]: the catalog holds $observed_collations of these columns, the table names $collation_n" >&2
  echo "  A new typed-id column or copy landed without a collation expectation here." >&2
  BAD=$((BAD + 1))
fi

gate_arm collations "$collation_n" 9

echo
echo "narrowing cases:        $narrowing_n"
echo "escalation cases:       $escalation_n"
echo "ladder authority cases: $ladder_n"
echo "collated columns:       $collation_n"

overall=0
[ "$BAD" -eq 0 ] || overall=1
gate_arms_finish || overall=1
if [ "$overall" -ne 0 ]; then
  echo "ORGANIZATION AUTHORITY GATE: FAILED" >&2
  exit 1
fi
echo "ORGANIZATION AUTHORITY GATE: PASSED"
