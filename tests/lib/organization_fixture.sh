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
#     INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id)
#       VALUES ('$APP', 'demo', 'free',
#               '$ZS_FIXTURE_PROJECT_ID', '$ZS_FIXTURE_ORGANIZATION_ID');
#     $(seat_app_owner_sql "$APP" "$CREATOR")
#     SQL
#
# BOTH OWNERSHIP COLUMNS, ALWAYS. `apps.organization_id` is NOT NULL and has no
# default, and the composite key `(project_id, organization_id) -> projects(id,
# organization_id)` is what stops the two from ever disagreeing - so a column
# list naming only `project_id` is not a partial fixture, it is a row PostgreSQL
# refuses:
#
#   ERROR: null value in column "organization_id" of relation "apps"
#          violates not-null constraint
#
# Native database tests in
# crates/zeroship-migrate-node/tests/platform_corpus/organization_authority.rs
# exercise missing and mismatched ownership columns against PostgreSQL.
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
# Keep reruns on the same rows. Hex digits belong to the base36 alphabet;
# leading zeros fill the canonical width and keep the body within the codec's
# numeric range. Native identity tests validate the emitted values.
organization_fixture_ids() {
  local slug="${1:?organization_fixture_ids needs a slug}" digest
  digest="$(printf '%s' "$slug" | md5sum | cut -c1-22)"
  ZS_FIXTURE_ORGANIZATION_ID="org_000${digest}"
  ZS_FIXTURE_PROJECT_ID="prj_000${digest}"
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

# seat_app_owner_sql <app-id> <user-id> [role]
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

# seat_app_owner <app-id> <user-id> <role> <psql-command> [args...]
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

# app_organization <app-id> <psql-command> [args...]
#
# Echo the organization id the app bills, or EXIT.
#
# THE READ IS THE POINT. `apps.organization_id` is where every billing table now
# hangs off - `organization_billing`, and through it
# `organization_billing_status`, `billing_customer_refs` and `invoices` - and a
# harness that creates its app through `POST /api/apps` never sees that id: the
# control plane mints the caller's personal organization and default project on
# first use and returns only the app. So the id has to come back out of the
# database, and an EMPTY read has to be loud.
#
# It would otherwise be silent in the worst way: `$(psql ... -tA -c 'SELECT
# organization_id ...')` over a missing app is an empty string, which then
# substitutes into the next INSERT as `organization_id = ''`. That is refused by
# a foreign key, far from here, with a message about a constraint rather than
# about the app id nobody checked. The token below can only appear when a row
# was really read.
#
# CALL IT WITH A GUARD. `seat_app_owner` can end the run from inside itself
# because it is called as a statement; this one is necessarily a command
# SUBSTITUTION, and `exit` inside `$( ... )` ends the substitution's subshell
# and nothing else. The harness then carries on with an empty variable, which is
# the failure this function exists to remove. So the call site owns the last
# step:
#
#     ORGANIZATION="$(app_organization "$APP" psql_exec)" || exit 1
#
# The diagnostic still reaches the terminal either way (it is written to stderr,
# which the substitution does not capture); the `|| exit 1` is what stops the
# run.
app_organization() {
  local app="${1:?app_organization needs an app id}"
  shift
  if [ "$#" -eq 0 ]; then
    echo "FAIL: app_organization needs the psql command to run, e.g. 'app_organization \"\$APP\" psql_exec'" >&2
    exit 1
  fi

  local out organization
  out="$("$@" -tA 2>&1 <<SQL
SELECT 'zs-app-org=' || organization_id FROM zeroship.apps WHERE id = '${app}';
SQL
)"
  organization="$(printf '%s\n' "$out" | sed -n 's/^zs-app-org=//p' | head -1)"
  if [ -n "$organization" ]; then
    printf '%s\n' "$organization"
    return 0
  fi

  echo "FAIL: app $app has no organization to bill." >&2
  echo "      apps.organization_id is NOT NULL and is written when the app is" >&2
  echo "      created, so an empty read means the app id is wrong or the app" >&2
  echo "      was never created. psql said:" >&2
  printf '%s\n' "$out" | sed 's/^/        /' >&2
  exit 1
}

# organization_billing_sql <organization-id>
#
# Emits the `organization_billing` row every other billing table hangs off.
#
# `organization_billing_status`, `billing_customer_refs` and `invoices` all
# carry a foreign key to it, so a harness that writes any of those without this
# first is refused - and the refusal names the constraint rather than the
# missing subject. `POST /api/apps` mints an organization but no billing row;
# only paying for something does that, and a harness that stubs the payment has
# to stub this too.
organization_billing_sql() {
  local organization="${1:?organization_billing_sql needs an organization id}"
  cat <<SQL
INSERT INTO zeroship.organization_billing (organization_id)
  VALUES ('${organization}') ON CONFLICT (organization_id) DO NOTHING;
SQL
}

# seat_organization_member_sql <organization-id> <user-id> [role]
#
# Seats <user> directly in <organization>, for a fixture that has an
# organization but NO app.
#
# The peer of `seat_app_owner_sql`, and the reason it is a separate emitter
# rather than a parameter: that one derives the organization from an app row and
# RAISES when the join is empty, which is the whole of its value. Here the
# organization id is what the caller already has, so the insert is a plain
# VALUES - a statement that either writes its row or errors, with no silent
# no-op to guard against.
#
# `role` must name a row in `zeroship.organization_roles`. The money-guarded
# routes (`/api/organizations/{id}/stripe/onboard`, `/connect/checkout`) need
# BillingWrite, which `owner` and `billing` carry and `developer` does not - so
# the default is the one that always works.
seat_organization_member_sql() {
  local organization="${1:?seat_organization_member_sql needs an organization id}"
  local user="${2:?seat_organization_member_sql needs a user id}"
  local role="${3:-owner}"
  cat <<SQL
INSERT INTO zeroship.organization_members (organization_id, user_id, role)
  VALUES ('${organization}', '${user}', '${role}')
  ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role;
SQL
}
