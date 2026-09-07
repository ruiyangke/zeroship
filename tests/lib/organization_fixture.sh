# shellcheck shell=bash
#
# The SQL an end-to-end harness needs to give an app an owner.
#
# WHAT CHANGED AND WHY THIS FILE EXISTS. `zeroship.app_members` is deleted. An
# app belongs to a project, a project belongs to an organization, and the people
# who answer for an app are that ORGANIZATION's members - one path,
# `apps.project_id -> projects.organization_id`, and no second one. Every
# harness that used to write a one-line `app_members` row now has to express
# that join, and sixteen copies of a three-table INSERT is sixteen chances to
# get it subtly wrong in a way that fails much later, as a 403 nobody can trace
# back to the fixture.
#
# THE FAILURE THIS FILE IS SHAPED AROUND. The natural spelling,
#
#     INSERT INTO zeroship.organization_members (organization_id, user_id, role)
#     SELECT p.organization_id, '<user>', 'owner'
#       FROM zeroship.apps a JOIN zeroship.projects p ON p.id = a.project_id
#      WHERE a.id = '<app>';
#
# inserts NOTHING, silently, when the app id is wrong or the app was never
# created - `INSERT ... SELECT` over an empty result is a successful statement
# that affects no rows. The harness then runs on, the bearer is refused
# somewhere far away, and the message says nothing about a missing seat. So the
# emitter below always pairs the insert with a check that RAISES, and the
# exception names the app and the user.
#
# Source it and call the emitters inside a psql heredoc:
#
#     . "$ROOT/tests/lib/organization_fixture.sh"
#     organization_fixture_ids billing-e2e          # <- IN THE PARENT SHELL
#     docker exec -i "$PG" psql -U "$U" -d "$DB" -v ON_ERROR_STOP=1 <<SQL || exit 1
#     $(organization_fixture_sql billing-e2e ops@zeroship.test)
#     INSERT INTO zeroship.apps (id, name, plan_id, project_id)
#       VALUES ('$APP', 'demo', 'free', '$ZS_FIXTURE_PROJECT_ID');
#     $(seat_app_owner_sql "$APP" "$CREATOR")
#     SQL
#
# THE COLUMN LIST ABOVE IS THE WHOLE OF IT. `apps.api_key` was dropped by
# db/migrations-ts/20260905000200_drop_app_api_key.ts, and this example named it
# until the drop and the organization change met in one tree. A column list that
# names a dropped column is refused at PARSE time, so the app row is never
# written and the seat below it then RAISEs about a missing app - which reads as
# a fault in THIS file rather than in the line the reader copied.
#
# WHEN THE SEAT IS THE ONLY STATEMENT, CALL `seat_app_owner` INSTEAD (below).
# The heredoc form above is for a seat that has to travel with the app row that
# precedes it; it is only as loud as the guard the caller puts on the heredoc.
#
# THE ID CALL IS SEPARATE, AND IT HAS TO BE. `$(organization_fixture_sql ...)`
# runs in a SUBSHELL, so any variable it sets is gone by the time the next line
# of the heredoc is expanded - the app row would be written with an empty
# `project_id` and refused by the shape CHECK, which is exactly what happened on
# this file's first run. `organization_fixture_ids` is what the parent shell
# calls; the emitter only reads.
#
# `-v ON_ERROR_STOP=1` is not optional. Without it psql reports the RAISE and
# carries on with an exit status of zero, which is the failure this file exists
# to remove.

# organization_fixture_ids <slug>
#
# Sets ZS_FIXTURE_ORGANIZATION_ID and ZS_FIXTURE_PROJECT_ID from <slug>.
#
# DETERMINISTIC, not random: a harness that reruns against a database it did not
# drop must land on the same rows, or the second run collides on the slug's
# unique index and the first run's fixture is still sitting there. The digest is
# hex, which is inside the `^(org|prj)_[0-9A-Za-z]{22}$` grammar the CHECK
# constraints enforce - a random id from `/dev/urandom` would need filtering to
# stay inside it, and a filter that occasionally emits 21 characters fails in
# one run out of many, which is the worst way for a fixture to fail.
organization_fixture_ids() {
  local slug="${1:?organization_fixture_ids needs a slug}" digest
  digest="$(printf '%s' "$slug" | md5sum | cut -c1-22)"
  ZS_FIXTURE_ORGANIZATION_ID="org_${digest}"
  ZS_FIXTURE_PROJECT_ID="prj_${digest}"
}

# organization_fixture_sql <slug> <billing-email>
#
# Emits the organization and its default project, idempotently, and leaves their
# ids in ZS_FIXTURE_ORGANIZATION_ID / ZS_FIXTURE_PROJECT_ID for the caller's own
# `INSERT INTO zeroship.apps`.
#
# Only harnesses that write an app row BY HAND need this. A harness that creates
# its apps through the control plane does not: `POST /api/apps` mints the
# caller's personal organization and default project when they have none, which
# is the zero-config first deploy and is exercised by every such harness whether
# it means to or not.
organization_fixture_sql() {
  local slug="${1:?organization_fixture_sql needs a slug}"
  local email="${2:?organization_fixture_sql needs a billing email}"
  # The caller must have run `organization_fixture_ids` in its OWN shell. If it
  # did not, refuse THROUGH THE SQL rather than on stderr: this function is
  # nearly always called inside `$(...)`, where a non-zero return is discarded
  # and a diagnostic on stderr is drowned by psql's own output, while an empty
  # expansion here would quietly emit a valid-looking script that inserts an
  # organization with no id.
  if [ -z "${ZS_FIXTURE_ORGANIZATION_ID:-}" ] || [ -z "${ZS_FIXTURE_PROJECT_ID:-}" ]; then
    printf 'DO $zs$ BEGIN RAISE EXCEPTION %s; END $zs$;\n' \
      "'harness fixture: organization_fixture_sql ran before organization_fixture_ids, so the ids are empty. Call organization_fixture_ids in the parent shell first.'"
    return 0
  fi
  cat <<SQL
INSERT INTO zeroship.organizations (id, slug, name, billing_email)
  VALUES ('${ZS_FIXTURE_ORGANIZATION_ID}', '${slug}', '${slug}', '${email}')
  ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.projects (id, organization_id, slug, name)
  VALUES ('${ZS_FIXTURE_PROJECT_ID}', '${ZS_FIXTURE_ORGANIZATION_ID}', 'default', 'Default')
  ON CONFLICT (id) DO NOTHING;
SQL
}

# seat_app_owner_sql <app-uuid> <user-uuid> [role]
#
# Seats <user> in the organization that owns <app>'s project, and refuses loudly
# if there is no such organization. This is the direct replacement for the
# deleted `INSERT INTO zeroship.app_members ... 'owner'`.
#
# `DO UPDATE`, not `DO NOTHING`: a harness that seats the same principal twice
# at two roles means the later call, and a silently kept earlier row would give
# it authority it did not ask for - or, worse, less.
seat_app_owner_sql() {
  local app="${1:?seat_app_owner_sql needs an app id}"
  local user="${2:?seat_app_owner_sql needs a user id}"
  local role="${3:-owner}"
  cat <<SQL
INSERT INTO zeroship.organization_members (organization_id, user_id, role)
  SELECT p.organization_id, '${user}', '${role}'
    FROM zeroship.apps a
    JOIN zeroship.projects p ON p.id = a.project_id
   WHERE a.id = '${app}'
  ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role;
DO \$zs\$
BEGIN
  IF NOT EXISTS (
    SELECT 1
      FROM zeroship.apps a
      JOIN zeroship.projects p ON p.id = a.project_id
      JOIN zeroship.organization_members m
           ON m.organization_id = p.organization_id AND m.user_id = '${user}'
     WHERE a.id = '${app}'
  ) THEN
    RAISE EXCEPTION 'harness fixture: app % has no project/organization to seat % on. The INSERT above matched no rows, which is a SUCCESSFUL statement affecting nothing - check the app id, and that the app was created before this ran.', '${app}', '${user}';
  END IF;
END
\$zs\$;
SQL
}

# seat_app_owner <app-uuid> <user-uuid> <role> <psql-command> [args...]
#
# Runs the seat and REFUSES TO BE QUIET ABOUT IT. `<psql-command>` is whatever
# the harness already uses to feed SQL to its database on stdin - the local
# `psql_exec` function in most of them, or the `docker exec -i ... psql ...`
# words spelled out.
#
# WHY THIS EXISTS ALONGSIDE THE EMITTER. `seat_app_owner_sql` ends in a RAISE so
# that a seat matching no rows is loud. Every standalone call site wrote it as
#
#     psql_exec >/dev/null 2>&1 <<SQL
#     $(seat_app_owner_sql "$APP" "$CREATOR")
#     SQL
#
# and every one of those three details throws the RAISE away: the message goes
# to /dev/null, the exit status is never read, and NO e2e harness in this tree
# sets `-e` (they all run `set -uo pipefail`), so a failing psql is simply the
# next line's predecessor. The apparatus was inert at the point it was built
# for. One of those harnesses even carried the comment "fails loudly if the app
# somehow has no project" directly above a call that could not.
#
# WHY IT DOES NOT TRUST THE EXIT STATUS ALONE. `ON_ERROR_STOP` lives in the
# CALLER's psql invocation, where this function cannot see it, and psql without
# it reports a RAISE and exits zero - the exact trap this file's header names.
# So the verdict is a token read back out of the database by the same
# invocation: the count can only be 1 if the row is really there, whatever psql
# decided to do about the statements above it.
#
# IT EXITS RATHER THAN RETURNING. A seat is a precondition, not an assertion;
# there is nothing useful to do with an app whose owner is missing, and a return
# code would land in the same unchecked place the redirection did.
seat_app_owner() {
  local app="${1:?seat_app_owner needs an app id}"
  local user="${2:?seat_app_owner needs a user id}"
  local role="${3:?seat_app_owner needs a role}"
  shift 3
  if [ "$#" -eq 0 ]; then
    echo "FAIL: seat_app_owner needs the psql command to run, e.g. 'seat_app_owner \"\$APP\" \"\$USER\" owner psql_exec'" >&2
    exit 1
  fi

  local sql out
  sql="$(seat_app_owner_sql "$app" "$user" "$role")
SELECT 'zs-seat-ok=' || count(*)::text
  FROM zeroship.apps a
  JOIN zeroship.projects p ON p.id = a.project_id
  JOIN zeroship.organization_members m
       ON m.organization_id = p.organization_id AND m.user_id = '${user}'
 WHERE a.id = '${app}' AND m.role = '${role}';"

  out="$("$@" 2>&1 <<<"$sql")"
  case "$out" in
    *"zs-seat-ok=1"*) return 0 ;;
  esac

  echo "FAIL: could not seat $user as '$role' on app $app." >&2
  echo "      The app must exist and reach an organization through" >&2
  echo "      apps.project_id -> projects.organization_id before this runs." >&2
  echo "      psql said:" >&2
  printf '%s\n' "$out" | sed 's/^/        /' >&2
  exit 1
}
