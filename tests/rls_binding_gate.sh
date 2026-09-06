#!/usr/bin/env bash
# Every row-level-security policy in db/migrations-ts/ must BIND A ROLE.
#
# THE QUESTION. A policy is a fence only if some role that actually reaches the
# table is SUBJECT to it. A role created with BYPASSRLS is exempt, so a policy
# whose only readers and writers are exempt roles is decoration: it is written,
# applied, visible in pg_policies, cited in reviews, and enforces nothing. The
# failure signature is a clean reading - the SQL is there, the policy is there,
# and no query is ever refused - which is why this has to be measured rather
# than read.
#
# WHAT IT RULES ON. For every table carrying a policy, the effective privileges
# each role holds on that table at the END of the whole corpus, and whether any
# such role lacks BYPASSRLS at its own final state. Grants are folded in file
# order because order is load-bearing here: a blanket REVOKE late in the corpus
# takes back an early grant, and reading either half alone gives the wrong
# answer.
#
# THIS GATE IS RED BY DESIGN until the sessions redesign lands
# (docs/proposals/2026-09-05-auth-foundation-redesign.md, fence F15 and step 0).
# The red IS the measurement: it names which policies are live fences and which
# are decoration, so that deleting the decoration is a recorded decision rather
# than a guess. It is deliberately NOT wired into CI while that is true; the
# row in tests/ci_wiring_gate.sh's allowlist says so and names the condition
# that removes it.
#
# WHY IT READS THE RECORDER AND NOT THE TEXT. The corpus builds grant targets
# from helpers and module constants, so a grep for a table name inside a
# grant() call misses real grants - and a missed grant makes a bound table read
# as unbound, which is a red for the wrong reason and indistinguishable from
# the red this gate exists to produce. The extractor below drains the same
# @zeroship/migrate recorder the applier drains, so it sees the ops that will
# actually run. It needs packages/zero-migrate/dist, which `pnpm build`
# produces; a missing one is a loud refusal, never a skip.
#
# WHAT IT DOES NOT RULE ON, stated so nobody reads it as complete:
#   - superusers, which bypass RLS unconditionally and are outside the corpus.
#   - the table OWNER, except that FORCE is reported per table: without FORCE
#     the owner is exempt, and the owner is whoever applied the migration.
#   - whether a policy's PREDICATE is correct. A policy binding a role while
#     comparing a column to a setting nothing sets is green here.
#   - roles created outside db/migrations-ts/ - per-app runtime roles are minted
#     by the migration service and never touch these tables.
#
# WHEN THE POLICIES ARE GONE, so is this gate. Arm `policy_binding` rules on
# policy-carrying tables; a corpus with none leaves it nothing to rule on, and
# the honest move then is to delete this file, not to lower its floor.
#
# Run the detector's own positive/control set: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

MIGDIR="$ROOT/db/migrations-ts"
RECORDER="$ROOT/packages/zero-migrate/dist/internal/recorder.js"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

# ---------------------------------------------------------------------------
# THE EXTRACTOR. Emits one FACT per line, and facts only - no verdicts. The
# judge below decides. Keeping the two apart is what lets the judge be driven
# by synthetic fact streams whose right answer is known, which is the only way
# to tell a detector that discriminates from one that always says "bound".
#
# Wire format, consumed by judge() and by the arms:
#   file     <basename> <ops>
#   role     <name> bypass=<0|1> explicit=<0|1>
#   rls      <schema.table> enabled=<0|1> forced=<0|1>
#   policy   <schema.table> <policy-name>
#   priv     <schema.table> <role> <comma-separated privileges>
#   raw      <classification>
#   control  <name> <pass|fail>
#   unparsed <text>
# ---------------------------------------------------------------------------
read -r -d '' EXTRACTOR_JS <<'JSEOF'
const fs = await import("node:fs");
const rec = await import("@zeroship/migrate/internal/recorder");

const dir = process.cwd();
const files = fs
  .readdirSync(dir)
  .filter((f) => /^[0-9]{14}_.*\.ts$/.test(f))
  .sort();

const out = [];
const emit = (s) => out.push(s);

const roles = new Map(); // name -> { bypass, inherit, explicit }
const memberOf = new Map(); // member -> Set(role it is a member of)
const privs = new Map(); // "schema.table|role" -> Set(privilege)
const rls = new Map(); // "schema.table" -> { enabled, forced }
const policies = [];
const unparsed = [];

const RLS_VERBS = ["select", "insert", "update", "delete"];

function role(name) {
  if (!roles.has(name)) roles.set(name, { bypass: false, inherit: true, explicit: false });
  return roles.get(name);
}
const unq = (s) => s.replace(/^"|"$/g, "");
const key = (t, r) => t + "|" + r;

function addPriv(t, r, ps) {
  if (ps.length === 0) return;
  const k = key(t, r);
  if (!privs.has(k)) privs.set(k, new Set());
  for (const p of ps) privs.get(k).add(p);
}
function delPriv(t, r, ps) {
  const k = key(t, r);
  if (!privs.has(k)) return;
  for (const p of ps) privs.get(k).delete(p);
  if (privs.get(k).size === 0) privs.delete(k);
}

// Only the four verbs RLS applies to. ALL expands to them; TRUNCATE,
// REFERENCES and TRIGGER are not row-level operations and are dropped, so a
// role holding only those does not count as reaching the table.
function privWords(text) {
  const bare = text.replace(/\([^)]*\)/g, " ");
  const found = new Set();
  for (const m of bare.matchAll(/\b(select|insert|update|delete|truncate|references|trigger|all)\b/gi)) {
    const w = m[1].toLowerCase();
    if (w === "all") for (const v of RLS_VERBS) found.add(v);
    else if (RLS_VERBS.includes(w)) found.add(w);
  }
  return [...found];
}
function roleList(text) {
  return text
    .replace(/\bwith\s+grant\s+option\b/gi, " ")
    .replace(/\bgranted\s+by\b.*$/gi, " ")
    .split(",")
    .map((s) => unq(s.trim()))
    .filter((s) => s.length > 0);
}

// classify() is PURE: it reads a statement and says what it is, touching no
// state. apply() is the only mutator. The split is what makes the controls at
// the bottom possible - they call classify() on statements whose answer is
// known, including the pair that differs in one character of prefix.
function classify(stmt) {
  const s = stmt.replace(/\s+/g, " ").trim().replace(/;$/, "").trim();
  if (!s) return { kind: "empty" };
  const relevant =
    /\bgrant\b/i.test(s) ||
    /\brevoke\b/i.test(s) ||
    /\balter\s+role\b/i.test(s) ||
    /\bcreate\s+role\b/i.test(s) ||
    /\bdrop\s+role\b/i.test(s) ||
    /\balter\s+default\s+privileges\b/i.test(s);
  if (!relevant) return { kind: "not_privilege" };

  let m;
  // Future relations only; it cannot change what an existing table grants.
  if (/^alter\s+default\s+privileges\b/i.test(s)) return { kind: "alter_default_privileges" };

  if ((m = s.match(/^alter\s+role\s+([A-Za-z0-9_"]+)\s+with\s+(.+)$/i))) {
    const attrs = m[2];
    const res = { kind: "alter_role", role: unq(m[1]) };
    if (/\bNOBYPASSRLS\b/i.test(attrs)) res.bypass = false;
    else if (/\bBYPASSRLS\b/i.test(attrs)) res.bypass = true;
    if (/\bNOINHERIT\b/i.test(attrs)) res.inherit = false;
    else if (/\bINHERIT\b/i.test(attrs)) res.inherit = true;
    return res;
  }
  if ((m = s.match(/^revoke\s+all\s+privileges\s+on\s+all\s+tables\s+in\s+schema\s+([A-Za-z0-9_"]+)\s+from\s+(.+)$/i))) {
    return { kind: "revoke_all_tables", schema: unq(m[1]), roles: roleList(m[2]) };
  }
  if (/^revoke\s+all\s+privileges\s+on\s+all\s+sequences\b/i.test(s)) {
    return { kind: "revoke_all_sequences" };
  }
  if ((m = s.match(/^grant\s+(.+?)\s+on\s+(?:table\s+)?([A-Za-z0-9_".]+)\s+to\s+(.+)$/i))) {
    if (!m[2].includes(".")) return { kind: "unparsed", text: s };
    return { kind: "grant_table", privileges: privWords(m[1]), object: unq(m[2]), roles: roleList(m[3]) };
  }
  if ((m = s.match(/^revoke\s+(.+?)\s+on\s+(?:table\s+)?([A-Za-z0-9_".]+)\s+from\s+(.+)$/i))) {
    if (!m[2].includes(".")) return { kind: "unparsed", text: s };
    return { kind: "revoke_table", privileges: privWords(m[1]), object: unq(m[2]), roles: roleList(m[3]) };
  }
  // Role membership carries no object, so a statement that still names one has
  // a shape the branches above did not recognise and must NOT fall through to
  // here. Without this guard `GRANT SELECT ON ALL TABLES IN SCHEMA s TO r` -
  // letters and spaces only - reads as a membership grant and its privileges
  // vanish silently, which is the one outcome this gate must never produce.
  const namesAnObject = /\bon\b/i.test(s);
  if (!namesAnObject && (m = s.match(/^grant\s+([A-Za-z0-9_", ]+)\s+to\s+(.+)$/i))) {
    return { kind: "grant_membership", parents: roleList(m[1]), roles: roleList(m[2]) };
  }
  if (!namesAnObject && (m = s.match(/^revoke\s+([A-Za-z0-9_", ]+)\s+from\s+(.+)$/i))) {
    return { kind: "revoke_membership", parents: roleList(m[1]), roles: roleList(m[2]) };
  }
  return { kind: "unparsed", text: s };
}

function apply(c, where) {
  switch (c.kind) {
    case "alter_role": {
      const r = role(c.role);
      if (c.bypass !== undefined) {
        r.bypass = c.bypass;
        r.explicit = true;
      }
      if (c.inherit !== undefined) r.inherit = c.inherit;
      break;
    }
    case "grant_table":
      for (const rn of c.roles) {
        role(rn);
        addPriv(c.object, rn, c.privileges);
      }
      break;
    case "revoke_table":
      for (const rn of c.roles) delPriv(c.object, rn, c.privileges);
      break;
    case "revoke_all_tables":
      for (const rn of c.roles) {
        for (const k of [...privs.keys()]) {
          if (k.startsWith(c.schema + ".") && k.endsWith("|" + rn)) privs.delete(k);
        }
      }
      break;
    case "grant_membership":
      for (const rn of c.roles) {
        role(rn);
        if (!memberOf.has(rn)) memberOf.set(rn, new Set());
        for (const p of c.parents) memberOf.get(rn).add(p);
      }
      break;
    case "revoke_membership":
      for (const rn of c.roles) {
        if (!memberOf.has(rn)) continue;
        for (const p of c.parents) memberOf.get(rn).delete(p);
      }
      break;
    case "unparsed":
      unparsed.push(where + ": " + c.text);
      break;
    default:
      break;
  }
}

function qual(schema, table, where) {
  if (!schema || !table) {
    unparsed.push(where + ": an op named a table with no schema");
    return null;
  }
  return schema + "." + table;
}

function applyDslGrant(on, privileges, targets, isGrant, where) {
  if (!on || on.kind !== "table") return; // schema and sequence grants are not table privileges
  if (!on.schema || !Array.isArray(on.names) || !Array.isArray(targets)) {
    unparsed.push(where + ": a table grant whose schema, names or grantees are not literal");
    return;
  }
  const ps = privWords((privileges || []).join(" "));
  for (const name of on.names) {
    for (const rn of targets) {
      role(rn);
      if (isGrant) addPriv(on.schema + "." + name, rn, ps);
      else delPriv(on.schema + "." + name, rn, ps);
    }
  }
}

for (const f of files) {
  const env = await rec.buildEnvelopeFromPath(dir + "/" + f, {});
  emit(`file ${f} ${env.ops.length}`);
  for (const op of env.ops) {
    switch (op.op) {
      case "createRole": {
        const r = role(op.name);
        if (Object.prototype.hasOwnProperty.call(op, "bypassRls")) {
          r.bypass = !!op.bypassRls;
          r.explicit = true;
        }
        break;
      }
      case "setRls": {
        const t = qual(op.schema, op.table, f);
        if (t) rls.set(t, { enabled: !!op.enabled, forced: !!op.forced });
        break;
      }
      case "createPolicy": {
        const t = qual(op.schema, op.table, f);
        if (t) policies.push({ table: t, name: op.name });
        break;
      }
      case "grant":
        applyDslGrant(op.on, op.privileges, op.to, true, f);
        break;
      case "revoke":
        applyDslGrant(op.on, op.privileges, op.from, false, f);
        break;
      case "raw": {
        for (const stmt of String(op.sql || "").split(";")) {
          const c = classify(stmt);
          if (c.kind === "empty" || c.kind === "not_privilege") continue;
          emit(`raw ${c.kind}`);
          apply(c, f);
        }
        break;
      }
      default:
        break;
    }
  }
}

// Privileges flow through role membership; BYPASSRLS does NOT - it is a role
// attribute, effective only for the role you are actually running as. A member
// declared NOINHERIT does not pick its parent's privileges up automatically.
let changed = true;
while (changed) {
  changed = false;
  for (const [member, parents] of memberOf) {
    if (!role(member).inherit) continue;
    for (const parent of parents) {
      for (const [k, set] of [...privs]) {
        if (!k.endsWith("|" + parent)) continue;
        const t = k.slice(0, k.length - parent.length - 1);
        const mk = key(t, member);
        const cur = privs.get(mk) || new Set();
        const before = cur.size;
        for (const p of set) cur.add(p);
        if (cur.size !== before) {
          privs.set(mk, cur);
          changed = true;
        }
      }
    }
  }
}

for (const [name, r] of [...roles].sort((a, b) => a[0].localeCompare(b[0]))) {
  emit(`role ${name} bypass=${r.bypass ? 1 : 0} explicit=${r.explicit ? 1 : 0}`);
}
for (const [t, s] of [...rls].sort((a, b) => a[0].localeCompare(b[0]))) {
  emit(`rls ${t} enabled=${s.enabled ? 1 : 0} forced=${s.forced ? 1 : 0}`);
}
for (const p of policies.slice().sort((a, b) => (a.table + a.name).localeCompare(b.table + b.name))) {
  emit(`policy ${p.table} ${p.name}`);
}
for (const [k, set] of [...privs].sort((a, b) => a[0].localeCompare(b[0]))) {
  const t = k.slice(0, k.lastIndexOf("|"));
  const r = k.slice(k.lastIndexOf("|") + 1);
  emit(`priv ${t} ${r} ${[...set].sort().join(",")}`);
}
for (const u of unparsed) emit(`unparsed ${u.replace(/\s+/g, " ")}`);

// THE CLASSIFIER'S OWN CONTROLS, run on every invocation rather than behind a
// flag. Each pair differs in ONE variable, and the first pair is the one that
// matters most: BYPASSRLS and NOBYPASSRLS differ by a prefix, and a word-blind
// match reads the second as the first - which would report every exempt role
// as bound and turn this whole gate green.
const controls = [
  ["alter_role_bypass", classify("ALTER ROLE r WITH LOGIN BYPASSRLS").bypass === true],
  ["alter_role_nobypass", classify("ALTER ROLE r WITH LOGIN NOBYPASSRLS").bypass === false],
  [
    "column_grant_is_a_table_grant",
    (() => {
      const c = classify("GRANT SELECT (a, b) ON zeroship.t TO r");
      return c.kind === "grant_table" && c.object === "zeroship.t" && c.roles[0] === "r" && c.privileges.join() === "select";
    })(),
  ],
  [
    "membership_is_not_a_table_grant",
    (() => {
      const c = classify("GRANT some_owner TO r");
      return c.kind === "grant_membership" && c.parents[0] === "some_owner" && c.roles[0] === "r";
    })(),
  ],
  [
    "blanket_revoke_seen",
    classify("REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM r").kind === "revoke_all_tables",
  ],
  // The sibling of the membership control, and the reason it is not enough on
  // its own: this statement is also letters and spaces, and reading it as a
  // membership grant would drop a privilege on every table in the schema.
  [
    "schema_wide_grant_is_refused",
    classify("GRANT SELECT ON ALL TABLES IN SCHEMA zeroship TO r").kind === "unparsed",
  ],
  [
    "ddl_is_not_a_privilege_statement",
    classify("CREATE TRIGGER x BEFORE UPDATE OF c ON zeroship.t FOR EACH ROW EXECUTE FUNCTION f()").kind ===
      "not_privilege",
  ],
];
for (const [name, ok] of controls) emit(`control ${name} ${ok ? "pass" : "fail"}`);

process.stdout.write(out.join("\n") + "\n");
JSEOF

# ---------------------------------------------------------------------------
# THE JUDGE. Facts in, one verdict line per policy-carrying table out. It is a
# function taking a FACT FILE so the controls below can hand it a stream whose
# right answer is known.
#
#   VERDICT <BOUND|UNBOUND> <table> bound=<roles|-> exempt=<roles|-> rls=<on|off> forced=<yes|no>
#   EDGES <n>
# ---------------------------------------------------------------------------
judge() {
  local facts="$1"
  local kind a b c
  declare -A bypass=() privs=() rls_on=() rls_forced=() policied=()

  while read -r kind a b c; do
    case "$kind" in
      role) bypass["$a"]="${b#bypass=}" ;;
      rls)
        rls_on["$a"]="${b#enabled=}"
        rls_forced["$a"]="${c#forced=}"
        ;;
      policy) policied["$a"]="yes" ;;
      priv) privs["$a|$b"]="$c" ;;
    esac
  done < "$facts"

  local edges=0 t k role_name bound exempt verdict on forced
  for t in $(printf '%s\n' "${!policied[@]}" | sort); do
    bound=""
    exempt=""
    for k in "${!privs[@]}"; do
      [ "${k%|*}" = "$t" ] || continue
      role_name="${k##*|}"
      edges=$((edges + 1))
      # An unknown role has no declared BYPASSRLS, which is what PostgreSQL
      # defaults to, so it counts as bound.
      if [ "${bypass[$role_name]:-0}" = "1" ]; then
        exempt="$exempt $role_name"
      else
        bound="$bound $role_name"
      fi
    done
    on="off"
    [ "${rls_on[$t]:-0}" = "1" ] && on="on"
    forced="no"
    [ "${rls_forced[$t]:-0}" = "1" ] && forced="yes"
    if [ "$on" = "off" ]; then
      verdict="UNBOUND"
    elif [ -z "$bound" ]; then
      verdict="UNBOUND"
    else
      verdict="BOUND"
    fi
    echo "VERDICT $verdict $t bound=$(list "$bound") exempt=$(list "$exempt") rls=$on forced=$forced"
  done
  echo "EDGES $edges"
}

list() {
  local s
  s="$(echo "$1" | tr ' ' '\n' | grep -v '^$' | sort | tr '\n' ',' | sed 's/,$//')"
  [ -n "$s" ] || s="-"
  echo "$s"
}

# ---------------------------------------------------------------------------
# DETECTOR CONTROLS. Four synthetic fact streams whose answer is known, each
# differing from its neighbour in ONE fact. Without these, a judge that always
# answered UNBOUND and a judge that measured something would print the same
# thing on a corpus that is mostly unbound - which this one is.
# ---------------------------------------------------------------------------
run_detector_controls() {
  local tmp="$1" ok=0 total=0 got
  local -a names=() results=()

  ctl() { # name, expected verdict, fact lines on stdin
    local name="$1" want="$2"
    cat > "$tmp/$name.facts"
    got="$(judge "$tmp/$name.facts" | awk '$1 == "VERDICT" { print $2 }')"
    total=$((total + 1))
    if [ "$got" = "$want" ]; then
      ok=$((ok + 1))
      results+=("ok   control $name: $want")
    else
      results+=("FAIL control $name: expected $want, judged ${got:-<nothing>}")
    fi
  }

  ctl exempt_reader_only UNBOUND <<'F'
role r_exempt bypass=1 explicit=1
rls s.t enabled=1 forced=1
policy s.t tenant_isolation
priv s.t r_exempt select,update
F

  # ONE VARIABLE against the case above: the reader's BYPASSRLS bit.
  ctl bound_reader BOUND <<'F'
role r_plain bypass=0 explicit=1
rls s.t enabled=1 forced=1
policy s.t tenant_isolation
priv s.t r_plain select,update
F

  # ONE VARIABLE against the case above: whether RLS is switched on at all.
  ctl rls_switched_off UNBOUND <<'F'
role r_plain bypass=0 explicit=1
rls s.t enabled=0 forced=0
policy s.t tenant_isolation
priv s.t r_plain select,update
F

  # ONE VARIABLE against the bound case: whether anybody reaches the table.
  ctl no_reader UNBOUND <<'F'
role r_plain bypass=0 explicit=1
rls s.t enabled=1 forced=1
policy s.t tenant_isolation
F

  printf '%s\n' "${results[@]}"
  echo "CONTROLS $ok $total"
}

# ---------------------------------------------------------------------------

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ "${1:-}" = "--self-test" ]; then
  echo "rls binding gate self-test"
  out="$(run_detector_controls "$TMP")"
  echo "$out" | grep -v '^CONTROLS ' | sed 's/^/  /'
  read -r _ c_ok c_total <<< "$(echo "$out" | grep '^CONTROLS ')"
  [ "$c_ok" = "$c_total" ] || exit 1
  exit 0
fi

gate_arms_init rls_binding

echo "rls binding gate"

# The gate cannot answer its question without the recorder the applier uses.
# Refusing here is the point: a textual fallback would read helper-built grant
# targets as absent and report a bound table as unbound.
[ -d "$MIGDIR" ] || { echo "gate cannot run: $MIGDIR is missing" >&2; exit 1; }
command -v node > /dev/null || { echo "gate cannot run: node is not on PATH" >&2; exit 1; }
if [ ! -f "$RECORDER" ]; then
  echo "gate cannot run: $RECORDER is missing." >&2
  echo "  It is built by \`pnpm build\`. This gate drains the same recorder the" >&2
  echo "  applier drains; there is no textual fallback, because a fallback that" >&2
  echo "  cannot resolve a helper-built grant target reports a bound table as" >&2
  echo "  unbound and looks exactly like a real finding." >&2
  exit 1
fi

FACTS="$TMP/facts"
if ! (cd "$MIGDIR" && node --input-type=module -e "$EXTRACTOR_JS") > "$FACTS" 2> "$TMP/err"; then
  echo "gate cannot run: the corpus extractor failed." >&2
  sed 's/^/    /' "$TMP/err" >&2
  exit 1
fi

# --- Arm 1: the corpus was actually read ----------------------------------
# A glob that stopped matching, or a recorder that returned nothing, leaves
# every later arm ruling on an empty world - and an empty world reports every
# policy as unbound, which is a red that looks exactly like the real one. The
# floor sits well under the corpus population: files are added a few at a time,
# and the failure this guards takes the number to zero.
CORPUS_FILE_FLOOR=20
n_files=$(grep -c '^file ' "$FACTS")
if ! gate_arm corpus_files "$n_files" "$CORPUS_FILE_FLOOR"; then
  fail "the extractor loaded $n_files migration file(s), under its floor.
       Everything below is a verdict about an empty corpus. Fix the
       enumeration; do not lower the floor."
else
  pass "the recorder drained $n_files migration file(s)"
fi

# --- Arm 2: the classifier still discriminates ----------------------------
# These are the extractor's own controls, run inside the extractor on every
# invocation. The floor is below the number declared because a control may
# legitimately be retired with the shape it guards; it is not below two,
# because the BYPASSRLS/NOBYPASSRLS pair is the one that cannot go.
PARSER_CONTROL_FLOOR=4
n_controls=$(grep -c '^control ' "$FACTS")
control_fails="$(awk '$1 == "control" && $3 != "pass" { print $2 }' "$FACTS" | tr '\n' ' ')"
if ! gate_arm parser_control "$n_controls" "$PARSER_CONTROL_FLOOR"; then
  fail "the SQL classifier ran $n_controls control(s), under its floor. Its
       readings below cannot be trusted."
elif [ -n "$control_fails" ]; then
  fail "the SQL classifier failed its own controls: $control_fails
       A classifier that cannot tell BYPASSRLS from NOBYPASSRLS reports every
       exempt role as bound and turns this gate green everywhere."
else
  pass "the SQL classifier passed all $n_controls of its own controls"
fi

# --- Arm 3: BYPASSRLS is declared somewhere and was seen ------------------
# Counts the roles whose exemption was decided by an EXPLICIT declaration - a
# create carrying the attribute, or an ALTER ROLE naming it - not the whole
# role universe. If the recorder's field name changes, or the ALTER ROLE
# statements are reshaped, this collapses while the role universe stays full.
ROLE_ATTRIBUTE_FLOOR=2
n_explicit=$(awk '$1 == "role" && $4 == "explicit=1"' "$FACTS" | wc -l | tr -d ' ')
exempt_roles="$(awk '$1 == "role" && $3 == "bypass=1" { print $2 }' "$FACTS" | paste -sd' ' -)"
if ! gate_arm role_attributes "$n_explicit" "$ROLE_ATTRIBUTE_FLOOR"; then
  fail "only $n_explicit role(s) carry an explicitly declared BYPASSRLS state,
       under the floor. The corpus declares exemptions somewhere; if this
       reads near zero the extractor has stopped seeing them, and every table
       below will read as bound."
else
  pass "explicit BYPASSRLS states read for $n_explicit role(s); exempt today: ${exempt_roles:-none}"
fi

# --- Arm 4: raw privilege SQL was parsed, not skipped ---------------------
# The corpus reaches for raw SQL where the DSL has no verb - role attributes,
# schema-wide revokes, column grants - and every one of those changes an
# answer here. An unparsed statement is a refusal, never a silent drop.
RAW_STATEMENT_FLOOR=6
n_raw=$(grep -c '^raw ' "$FACTS")
n_unparsed=$(grep -c '^unparsed ' "$FACTS")
if ! gate_arm raw_privilege_sql "$n_raw" "$RAW_STATEMENT_FLOOR"; then
  fail "only $n_raw raw privilege statement(s) were classified, under the floor.
       The corpus reaches for raw SQL to alter role attributes and to revoke
       schema-wide; if this collapsed, those changes are invisible here."
elif [ "$n_unparsed" -gt 0 ]; then
  fail "$n_unparsed privilege statement(s) could not be parsed:
$(sed -n 's/^unparsed /         /p' "$FACTS")
       Each one may grant or revoke access to a policy-carrying table. Teach
       classify() the shape rather than letting it fall through."
else
  pass "all $n_raw raw privilege statement(s) classified, none unparsed"
fi

# --- Arm 5: the detector discriminates, proven here, not asserted ---------
# Four synthetic fact streams, each differing from a neighbour in one fact.
# This is what separates a judge that measures from one that always answers
# UNBOUND - and on a corpus that is mostly unbound, those two print the same
# thing. The floor is one under the set because a shape may be retired, but
# the exempt/bound pair cannot be.
DETECTOR_CONTROL_FLOOR=3
ctl_out="$(run_detector_controls "$TMP")"
read -r _ ctl_ok ctl_total <<< "$(echo "$ctl_out" | grep '^CONTROLS ')"
if ! gate_arm detector_control "${ctl_total:-0}" "$DETECTOR_CONTROL_FLOOR"; then
  fail "the judge ran ${ctl_total:-0} control(s), under its floor."
elif [ "$ctl_ok" != "$ctl_total" ]; then
  fail "the judge failed its own controls:
$(echo "$ctl_out" | sed -n 's/^FAIL /         /p')"
else
  pass "the judge answered all $ctl_total controls correctly, both directions"
fi

# --- Arm 6: every policy binds a role ------------------------------------
# The gate's actual question. Counts the (policy table, role) pairs the
# verdicts rest on: if the grant fold collapsed, every table reads unbound and
# this number - not the verdict - is what says so.
PRIVILEGE_EDGE_FLOOR=6
VERDICTS="$TMP/verdicts"
judge "$FACTS" > "$VERDICTS"
n_edges=$(awk '$1 == "EDGES" { print $2 }' "$VERDICTS")
n_tables=$(grep -c '^VERDICT ' "$VERDICTS")
if ! gate_arm privilege_edges "${n_edges:-0}" "$PRIVILEGE_EDGE_FLOOR"; then
  fail "the policy-carrying tables carry ${n_edges:-0} role privilege edge(s),
       under the floor. Every verdict below would read UNBOUND whether or not
       the policies bind anything."
else
  pass "${n_edges} role privilege edge(s) reach the policy-carrying tables"
fi

echo
echo "  per-table verdict:"
sed -n 's/^VERDICT /    /p' "$VERDICTS" | awk '{ printf "    %-8s %-32s %s %s %s %s\n", $1, $2, $3, $4, $5, $6 }'
echo

POLICY_TABLE_FLOOR=5
unbound="$(awk '$1 == "VERDICT" && $2 == "UNBOUND" { print $3 }' "$VERDICTS" | paste -sd' ' -)"
if ! gate_arm policy_binding "$n_tables" "$POLICY_TABLE_FLOOR"; then
  fail "only $n_tables policy-carrying table(s) were ruled on, under the floor.
       Either the corpus stopped declaring policies - in which case this gate
       has no subject and should be deleted - or the extraction broke."
elif [ -n "$unbound" ]; then
  fail "these policies bind NO role that reaches their table:
         $unbound
       Every role holding a privilege on them carries BYPASSRLS, or the table
       has no reader at all, or its RLS is switched off. The policy is applied,
       visible in pg_policies, and refuses nothing. Either give the readers a
       non-exempt database identity, or delete the policy and say in its place
       what actually keeps tenants apart."
else
  pass "all $n_tables policy-carrying table(s) bind at least one non-exempt role"
fi

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  rls binding gate: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
