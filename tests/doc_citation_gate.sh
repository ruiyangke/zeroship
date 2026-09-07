#!/usr/bin/env bash
#
# Every code path a document points at must exist, and every cited line must
# land inside its file.
#
# WHAT THIS GATE DOES NOT CATCH, measured 2026-08-29. It rules on the PATH and
# on the line being within the file, never on the line being the RIGHT one. Two
# design reviewers independently found four citations in the db proposal set
# that each pointed one line above their symbol - RESERVED_ENV_DB_NAMES cited
# at :1136 when it is declared at :1137, and the same offset on three more.
# Every one of them passed this gate green, because every one named a real file
# and a line inside it.
#
# A line-accuracy arm was measured and REJECTED rather than skipped. Checking
# that a cited line still contains its symbol needs the symbol, and only 5 of
# the 94 path:line citations in those two documents put a backticked identifier
# adjacent to the citation - the rest wrap across lines or name a phrase. The
# pattern that found those 5 also found 0 in data-system.md, a file that
# demonstrably contains such a citation, so the measurement was of the regex
# rather than of the docs. The alternative - recording each cited line's
# content so drift is detectable - is a census of expected values, which is the
# thing this repo's gate discipline exists to refuse.
#
# So line numbers in prose are drift-prone BY CONSTRUCTION and this gate does
# not pretend otherwise. The path is the durable claim; the line is a courtesy.
# If a citation's line matters to an argument, quote the code instead.
#
# RE-MEASURED AND RE-REJECTED 2026-09-04, on a much worse failure than the one
# above. AGENTS.md's schema-epoch paragraph carried TEN line citations and SEVEN
# were wrong; docs/architecture/data-system.md carried four over the same code
# and three were wrong, DIFFERENTLY - two documents drifting independently, not
# one copy-paste. Every one passed this gate.
#
# The past-EOF check did not catch a single one, and could not have: all seven
# named a line INSIDE the file. Do not propose it as the fix - it already ships.
#
# SYMBOL ADJACENCY WOULD HAVE CAUGHT 3 OF 4 of the ones with a backticked
# identifier beside them, which is a better hit rate than the 2026-08-29 note
# implies. It is still rejected, on the ARM CONTRACT rather than the hit rate:
# only 5 of 94 path:line citations put a backticked identifier adjacent, so an
# arm built on it rules on ~5 items and can carry a floor of at most 2. A floor
# of 2 does not separate "clean" from "did not look" - one reworded sentence
# takes it to 3, then to 1, then someone lowers the floor. That is precisely the
# decay tests/lib/gate_arms.sh exists to prevent. The paragraph that motivated it
# is also unrepresentative BECAUSE someone was arguing from it, so it is densely
# symbol-annotated; sizing a gate on it means sizing on the best-written prose in
# the tree.
#
# The fourth citation had no adjacent symbol at all - it is followed by a FENCED
# QUOTE - and a "the fence must appear in the cited range" rule is defeated here
# anyway: rustfmt has since wrapped one of those three "verbatim" lines across
# three lines. A verbatim-quote check goes red on reformatting.
#
# RESOLVING A BARE `:NNN` AGAINST THE LAST FULL PATH IS ALSO REJECTED, and it is
# the worse idea of the two. Measured on that same paragraph: `:1015` follows a
# `reducer/tests.rs:1679` citation, so last-path-wins binds it to tests.rs while
# the author meant reducer/mod.rs. A gate that confidently names the wrong file
# is how gates get switched off.
#
# THE FAILURE THIS EXISTS FOR, measured 2026-08-28. The zeroship- crate rename
# moved every crate to a `zeroship-` prefix and nothing re-read the prose that
# pointed at them. 1082 of 1497 distinct code paths named under docs/ resolved
# to nothing. AGENTS.md - the file every agent loads first - carried 28 dead
# paths, including line 489, which instructs the reader to run
#     ./crates/runtime/tests/setup-wpt.sh
# a file that has not existed under that name for weeks. An agent following the
# documented setup gets "No such file or directory", which reads as a broken
# REPO rather than a stale DOC, so the cost is paid by whoever trusts the
# documentation most.
#
# It went unnoticed because a rename is mechanically safe for CODE - the
# compiler finds every caller - and mechanically invisible for PROSE. Nothing
# in the tree read documentation as if it made checkable claims. This gate
# does, so the next rename cannot be half-applied in silence.
#
# CURRENT SCOPE AND EXPLICIT HISTORICAL EXEMPTIONS. This gate covers AGENTS.md,
# the named proposal/design set, docs/feature-map.md, docs/runbooks/*.md,
# docs/build-and-deploy-golden-path.md, and docs/reference/*.md. It does not
# silently treat every other document as live.
#
# `docs/decisions/` is an immutable record of what was true when each ADR
# landed. `docs/archive/` is superseded material retained deliberately. A dead
# path in either directory may be historically correct, so both are EXEMPT by
# policy. `check_citations` enforces that exemption even if a future caller
# hands it a broad glob. Other documents remain outside this gate's declared
# scope until they are deliberately cleaned and added.
#
# THE `DELETED` ESCAPE. A document may legitimately cite a file that the change
# it describes went on to delete - a design doc naming the code it replaced.
# Such a citation passes only if the citing line also says DELETED, so the
# author has to state the fact rather than leave a pointer that silently rots.
#
# Run the harness's own planted cases and their controls: this script --self-test.
# It rules on the INSTRUMENT and touches no document; the bare run rules on the
# tree. CI runs both, as two steps, for the reason the self-test header gives.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init doc_citation

FAILED=0

# Paths AGENTS.md names that are BUILD OUTPUTS: absent from a clean checkout,
# present after the command that makes them. Whether they exist is a fact about
# the MACHINE, not about the document, so a gate that ruled on them would pass
# or fail on who ran what.
#
# Two are named here because they are absent even on a fully set-up tree unless
# you ran their specific script (measured 2026-09-04: both are absent right now),
# and git has never heard of them either way.
#
# The rest is DERIVED: `git check-ignore` is the repository stating outright that
# it does not track a path. That arm replaced nothing - it was simply missing,
# and two citations were being ruled on by whether this machine had run a setup
# script. Measured 2026-09-04, `crates/zeroship-runtime/tests/wpt/` (fetched by
# setup-wpt.sh and gitignored) and the db SDK's generated `internal.js` bundle
# (a pnpm build output) were both on disk here and would have failed a clean
# checkout. The second is ALREADY an ALLOW row in tests/source_citation_gate.sh
# for exactly this reason, so the two gates were giving opposite verdicts on one
# citation - and writing its path out here made a third gate refuse this file,
# which is the same rule working from the other side.
is_generated_artifact() {
  case "$1" in
    sdks/ui/coverage/*|sdks/ui/coverage) return 0 ;;   # pnpm test-storybook:coverage
    tests/data/live/tls_live.conf) return 0 ;;         # libs/compio-postgres/tests/tls_live_setup.sh
  esac
  git check-ignore -q "$1" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Arm 1 - every literal path AGENTS.md names exists.
# ---------------------------------------------------------------------------
agents_examined=0
agents_bad=0
# The character class DELIBERATELY includes `{},` so a brace form such as
# `crates/plugin-{db,kv,storage}/` is captured WHOLE and then skipped below.
# Excluding those characters instead truncates it to `crates/plugin-`, which is
# not a brace form, is not skipped, and is reported as a missing path.
#
# `set -f` BEFORE THE LOOP, and it is not tidiness. `for p in $(...)` unquoted
# runs PATHNAME EXPANSION on the extracted tokens, so a token carrying `*`
# never reached the "brace/glob forms are prose" skip on the line below - the
# shell had already replaced it with its filesystem expansion. Measured
# 2026-09-04: `sdks/*` and `crates/zeroship-migrate*/` expanded into 30 extra
# entries, taking this arm's declared count from 113 to 143. Every one of the
# 30 came OFF THE FILESYSTEM, so every one passed the `-e` test by
# construction: 21% of what this arm said it ruled on was items that could not
# fail. Brace forms were never at risk - brace expansion is not applied to the
# result of a command substitution - which is why the comment above reasoned
# about `{}` and not about `*`.
set -f
for p in $(grep -oE '(crates|libs|sdks|db|tests|deploy|policies|schema|examples|docs)/[A-Za-z0-9_./{},*-]+' AGENTS.md \
           | tr -d '`' | sed 's/[.,)]*$//' | sort -u); do
  case "$p" in *[{}\*]*) continue ;; esac      # brace/glob forms are prose
  is_generated_artifact "$p" && continue
  agents_examined=$((agents_examined + 1))
  if [ ! -e "$p" ]; then
    echo "AGENTS.md names a path that does not exist: $p" >&2
    agents_bad=$((agents_bad + 1))
  fi
done
set +f
gate_arm agents_md_paths "$agents_examined" 40 || FAILED=1
[ "$agents_bad" -eq 0 ] || FAILED=1

# ---------------------------------------------------------------------------
# Arm 2 - every file citation in the proposals resolves, and a cited line is
# inside the file. A past-EOF citation is the quieter half: the path looks
# right, so a reader who does not open it believes the claim is anchored.
# ---------------------------------------------------------------------------

# Sets `cites_examined` and `cites_bad` for the documents passed in.
#
# Longer extensions come FIRST so the alternation cannot truncate `Foo.tsx` to
# `Foo.ts` or `config.jsonc` to `config.json`. The former blind spot silently
# skipped line citations; the same truncation in a measurement script invented
# 143 missing paths under sdks/ui that were never wrong.
#
# Extract the whole path-shaped token before selecting repository roots. A
# regex that begins at `schema/` also finds that suffix inside the shorthand
# `zeroship-schema/src/query.rs`; that is not a repository-root citation.
# How a file's text is fed to the extractor. Default: verbatim.
#
# `CITE_SOURCE` exists because the extractor is LINE-BASED and one class of
# citation is not. See `unwrap_comment_continuations` below; a caller sets this
# for one arm and resets it, so no other arm's counts can move underneath it.
CITE_SOURCE=cat

# Rejoin a `//` comment citation that WRAPPED ACROSS TWO LINES.
#
# THIS IS NOT A CONVENIENCE. On 2026-09-04 a sweep repaired two of three dead
# `crates/zeroship-migrate-adapter/...` citations in db/migrations-ts/ and missed
# the third - the one asserting a SECURITY property, that the worker holds no
# write privilege in `zeroship`. It survived because its path breaks after
# `.../tests/` and continues `platform_migrate.rs` on the next line. Both this
# gate and tests/source_citation_gate.sh extract per line, so the head has no
# extension (no match) and the tail has no repository-root prefix (filtered out):
# the file yields ZERO citations while its two siblings yield theirs.
#
# THE JOIN SET IS `/` AND `_` ONLY, and the exclusion of `-` is deliberate: `--`
# is this repo's ASCII em-dash substitute and ends comment lines legitimately.
# Measured 2026-09-04 over all 37 files / 3233 lines of db/migrations-ts: THREE
# join sites. Two are real wrapped citations; the third is prose ("the two
# lookups every disable / anonymize /") which joins to a token carrying no
# repository-root prefix and so yields no citation.
#
# The join adds NO separator, because a path is being reassembled, not a
# sentence. The continuation's `// ` prefix is stripped first.
#
# IT IS STRICTLY ADDITIVE - the raw file FIRST, then the joined lines - and that
# is a correction, not a flourish. The first version emitted ONLY the joined
# text and HID FIVE CITATIONS that resolve today, among them
# `crates/zeroship-auth/src/cron/token_sweep.rs:125`. Joining with no separator
# glues the previous line's tail onto the path, so a token that began `crates/`
# now begins `...something/crates/` and fails the repository-root prefix test.
# Measured 2026-09-04: raw 28 citations, join-only 23, ADDED by the join 0,
# HIDDEN by it 5 - all five present on disk. It replaced coverage rather than
# extending it, and because all five still resolved the arm stayed green while
# quietly ceasing to watch them. Emitting both makes the pre-pass incapable of
# subtracting; duplicate citations are harmless, the caller sorts unique.
unwrap_comment_continuations() {
  cat "$1"
  awk '
    /^[[:space:]]*\/\// {
      line = $0
      if (held != "") {
        sub(/^[[:space:]]*\/\/[[:space:]]?/, "", line)
        line = held line
        held = ""
      }
      if (line ~ /[\/_]$/) { held = line; next }
      print line
      next
    }
    { if (held != "") { print held; held = "" } print }
    END { if (held != "") print held }
  ' "$1"
}

check_citations() {
  cites_examined=0
  cites_bad=0
  local f cite path line eof
  for f in "$@"; do
    # Historical documents preserve their contemporary citations. Keep this
    # executable exemption beside the scope policy above so a broad future
    # glob cannot silently turn either directory into a live-doc arm.
    case "$f" in
      docs/decisions/*|docs/archive/*) continue ;;
    esac
    [ -f "$f" ] || continue
    for cite in $("$CITE_SOURCE" "$f" \
                  | grep -oE '[A-Za-z0-9_./-]+\.[A-Za-z0-9]+(:[0-9]+(-[0-9]+)?)?' \
                  | sed -E 's#^((\.\.?)/)+##' \
                  | grep -E '^(crates|libs|sdks|tests|db|deploy|policies|schema|examples|docs)/[A-Za-z0-9_./-]+\.(tsx|jsx|jsonc|mjs|cjs|json|rs|ts|js|sh|toml|md)(:[0-9]+(-[0-9]+)?)?$' \
                  | sort -u); do
      case "$cite" in
        *:[0-9]*)
          path="${cite%:*}"
          line="${cite##*:}"
          # A RANGE IS TESTED AT ITS END, not its start. `(:[0-9]+(-[0-9]+)?)?`
          # captures `:325-327` whole; taking only 325 would pass a citation
          # whose END is past EOF, which is the half that misleads - the reader
          # believes the whole quoted span is anchored.
          line="${line##*-}"
          ;;
        *)
          path="$cite"
          line=""
          ;;
      esac
      # A GENERATED, GITIGNORED ARTIFACT IS NOT A BROKEN CITATION, and this arm
      # used to say it was. `is_generated_artifact` was defined for arm 1 and
      # called from arm 1 ALONE, so the same path was skipped when AGENTS.md
      # named it and refused when a reference doc did - one repository, two
      # verdicts, decided by which file the citation happened to sit in.
      #
      # The instance that surfaced it: `docs/reference/env-vars.md` cites
      # `deploy/ops/zeroship.test.toml`, which `tests/provision_test_backends.sh`
      # writes and `.gitignore` covers, and the citing passage says so two
      # paragraphs later. The DELETED escape was the only exit this arm offered
      # and it would have been a lie - the file is generated, not deleted, and a
      # reader following that word would go looking for the commit that removed
      # it.
      #
      # Skipped BEFORE the counter so the arm does not claim to have ruled on an
      # item it cannot fail, which is the defect arm 1's own comment records.
      is_generated_artifact "$path" && continue
      cites_examined=$((cites_examined + 1))

      # Every occurrence must say DELETED on its own line. One historical use
      # must not exempt a second, live use of the same path elsewhere in a doc.
      #
      # Read through CITE_SOURCE, not the raw file: a citation that only EXISTS
      # after unwrapping has no raw line to carry its DELETED, so checking the
      # raw file here would make the escape unreachable for exactly the citations
      # the unwrap exists to surface.
      if "$CITE_SOURCE" "$f" | grep -F "$cite" >/dev/null \
          && ! "$CITE_SOURCE" "$f" | grep -F "$cite" | grep -qv 'DELETED'; then
        continue
      fi

      if [ ! -f "$path" ]; then
        echo "$f cites a file that does not exist: $cite" >&2
        echo "  (if the file was deleted on purpose, say DELETED on that line)" >&2
        cites_bad=$((cites_bad + 1))
        continue
      fi
      if [ -n "$line" ]; then
        eof=$(wc -l < "$path")
      fi
      if [ -n "$line" ] && [ "$line" -gt "$eof" ]; then
        echo "$f cites $cite but that file has only $eof lines" >&2
        cites_bad=$((cites_bad + 1))
      fi
    done
  done
}

# ---------------------------------------------------------------------------
# `--self-test` - plant citations this gate MUST refuse, and prove it does.
#
# WHY THIS GATE NEEDS ONE. Every arm passes at zero bad citations, which is also
# what an extractor that stopped matching prints. The floors catch a collapse to
# NOTHING; they cannot catch the loss of ONE CAPABILITY inside a population that
# is mostly something else. Measured 2026-09-04: delete the `(:[0-9]+(-[0-9]+)?)?`
# group from the extractor and the root filter above - which removes every line
# check this gate performs, on every document - and `agents_md_citations` falls
# from 77 to 76 and stays GREEN at floor 30, because only 7 of its 77 citations
# carry a line at all. Raising that floor to 7 was considered and rejected:
# ordinary editing of AGENTS.md moves that number, and a floor sized to the thing
# it protects is the fragile shape tests/lib/gate_arms.sh warns about. So the
# capability is bound HERE, by a planted citation a line-blind extractor cannot
# refuse, rather than by a proxy count.
#
# EVERY CASE HAS A CONTROL DIFFERING IN ONE VARIABLE, and the controls are the
# half that makes the refusals mean something: a harness that refuses everything
# passes every refusal case and is worthless. The past-EOF pair differs by ONE in
# the line number (eof+1 against eof), which also pins the boundary at `>` and
# not `>=`.
#
# THE ABSENT ANCHORS ARE BUILT AT RUN TIME, NOT WRITTEN OUT. A literal dead path
# under `tests/`, `docs/` or `crates/` in this file would be a real finding for
# tests/source_citation_gate.sh, which scans this directory - a detector that
# fires on its own probe reports nothing but itself.
#
# WHAT THIS STILL DOES NOT BIND. A citation naming a REAL line that says
# something else: all seven wrong citations corrected in AGENTS.md on 2026-09-04
# were of exactly that kind, and nothing here closes it - see the arm-1b header.
# Arm 1's `-e` sweep is also unbound, because it reads the literal filename
# AGENTS.md and so cannot be pointed at a fixture.
#
# THE CODE-FENCE CASE IS NOT HERE, AND IT CANNOT BE. This gate has no notion of
# a fence: it extracts citations from a document's whole text, so an unclosed
# fence changes nothing it does and there is no state for a fixture to invert.
# The arm that DOES invert on one is `doc_cargo_selectors` in
# tests/cargo_package_spec_gate.sh, whose `fenced_only` flips in-block state for
# the rest of a file; both halves are addressed there rather than mimicked here -
# its self-test now runs in CI and goes red on the inverted filter, and the cause
# the self-test cannot reach, an unclosed fence in a live DOCUMENT, is refused by
# a fence-balance arm over the whole live set.
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  echo "== self-test: planted citations, each with a one-variable control =="

  # Arm 1 has already run: it sits above `check_citations`, which is what this
  # harness drives. Its TREE verdict is dropped here on purpose - a dead path in
  # AGENTS.md is a finding for the bare run, which CI runs as its own step, and
  # reporting it here would say the instrument is broken when it is working. Its
  # ARM refusal is NOT dropped: a collapse of arm 1's enumeration IS an
  # instrument failure, and gate_arms_finish still carries it.
  FAILED=0

  probe="$(mktemp -d "${TMPDIR:-/tmp}/zsdoccite.XXXXXX")" || exit 1
  trap 'rm -rf "$probe"' EXIT

  st_ok()  { printf '  ok   %s\n' "$1"; }
  st_bad() { printf '  FAIL %s\n' "$1"; FAILED=1; }

  # Anchors. Present ones are real files; absent ones carry `$$` so this file
  # contains no dead citation of its own, and so two runs cannot collide.
  a_sh="tests/lib/gate_arms.sh"
  a_md="docs/reference/db.md"
  a_rs="crates/zeroship-data-engine/src/lib.rs"
  gone_md="docs/reference/absent-$$.md"
  gone_rs_dir="crates/zeroship-data-engine/src/transaction/"
  gone_rs_tail="absent-$$.rs"
  for a in "$a_sh" "$a_md" "$a_rs"; do
    [ -f "$a" ] || { echo "self-test anchor is missing: $a" >&2; exit 1; }
  done
  for g in "$gone_md" "$gone_rs_dir$gone_rs_tail"; do
    [ -e "$g" ] && { echo "self-test anchor exists and must not: $g" >&2; exit 1; }
  done
  eof_sh=$(wc -l < "$a_sh")

  # Drive check_citations over one planted document and rule on its verdict.
  # The arm's `examined` is the number of citations THE FIXTURE YIELDED, so a
  # fixture the extractor stopped seeing fails on its floor rather than passing
  # as "nothing to refuse" - the same contract the real arms carry.
  #
  #   st_case <arm> <floor> refuse|accept <fixture> <description>
  st_case() {
    local arm="$1" floor="$2" expect="$3" doc="$4" what="$5"
    check_citations "$doc" 2>/dev/null
    if ! gate_arm "$arm" "$cites_examined" "$floor"; then
      st_bad "$what: the fixture yielded $cites_examined citation(s), so this case proves nothing"
      return
    fi
    case "$expect" in
      refuse)
        if [ "$cites_bad" -ge 1 ]; then
          st_ok "$what: REFUSED ($cites_bad bad of $cites_examined examined)"
        else
          st_bad "$what: ACCEPTED - $cites_examined citation(s) examined, none refused"
        fi
        ;;
      accept)
        if [ "$cites_bad" -eq 0 ]; then
          st_ok "$what: accepted ($cites_examined examined)"
        else
          st_bad "$what: REFUSED $cites_bad of $cites_examined - the control must stay clean"
        fi
        ;;
    esac
  }

  # 1+2. PAST EOF, and its control one line lower. This is the pair that binds
  # the line check itself: with the line group deleted from the extractor, the
  # first fixture yields a citation to a file that EXISTS and is accepted.
  printf 'The floor lives at `%s:%d`.\n' "$a_sh" "$((eof_sh + 1))" > "$probe/past_eof.md"
  printf 'The floor lives at `%s:%d`.\n' "$a_sh" "$eof_sh" > "$probe/at_eof.md"
  st_case selftest_past_eof 1 refuse "$probe/past_eof.md" "a line one past end-of-file"
  st_case selftest_at_eof   1 accept "$probe/at_eof.md"   "control: the last line of the same file"

  # 3+4. A RANGE IS TESTED AT ITS END. Same one-variable pair, moved to the end
  # of a range: a check that read the START would accept the first fixture, and
  # the reader would believe the whole quoted span is anchored.
  printf 'Quoted at `%s:1-%d`.\n' "$a_sh" "$((eof_sh + 1))" > "$probe/range_past.md"
  printf 'Quoted at `%s:1-%d`.\n' "$a_sh" "$eof_sh" > "$probe/range_at.md"
  st_case selftest_range_past 1 refuse "$probe/range_past.md" "a range ending past end-of-file"
  st_case selftest_range_at   1 accept "$probe/range_at.md"   "control: the same range ending at EOF"

  # 5+6. THE POSITIVE CONTROL FOR THE HARNESS ITSELF: a path that does not
  # exist. If this one ever passes, nothing else here means anything. The `.md`
  # anchors also bind the document half of the extension alternation.
  printf 'See `%s` for the contract.\n' "$gone_md" > "$probe/absent.md"
  printf 'See `%s` for the contract.\n' "$a_md" > "$probe/present.md"
  st_case selftest_absent_path  1 refuse "$probe/absent.md"  "a cited document that does not exist"
  st_case selftest_present_path 1 accept "$probe/present.md" "control: the same sentence naming a real document"

  # 7+8. THE `DELETED` ESCAPE, both directions. A doc may cite a file the change
  # it describes deleted, but EVERY occurrence must say so - one escaped mention
  # must not cover a second, live one elsewhere in the same document.
  printf 'The old surface `%s` was DELETED on 2026-08-28.\n' "$gone_md" > "$probe/deleted.md"
  printf 'The old surface `%s` was DELETED on 2026-08-28.\nStill read `%s` first.\n' \
    "$gone_md" "$gone_md" > "$probe/deleted_partial.md"
  st_case selftest_deleted_escape  1 accept "$probe/deleted.md"         "a dead path whose only mention says DELETED"
  st_case selftest_deleted_partial 1 refuse "$probe/deleted_partial.md" "control: a second, unescaped mention of it"

  # 9+10. THE WRAPPED-COMMENT PRE-PASS, which the arm-7 header says is proven by
  # mutation rather than by a number. Here it is proven by both, permanently:
  # ONE fixture read two ways. Under `unwrap_comment_continuations` the citation
  # split across two `//` lines is rejoined and refused; under plain `cat` the
  # head has no extension and the tail no repository root, so the file yields
  # only its unwrapped neighbour - which is the blindness the pre-pass removes.
  #
  # THE FIXTURE'S THIRD LINE ENDS IN `/` ON PURPOSE, and without it this pair is
  # weaker than it looks. The pre-pass is ADDITIVE - raw text first, then the
  # joined lines - because an earlier join-only version HID five citations that
  # resolve: gluing a prose line's tail onto a path makes the token start with
  # that tail and fail the repository-root test. A fixture whose live citation is
  # not preceded by a joinable line cannot tell additive from join-only. This one
  # is, so dropping the raw half takes the join arm to one citation, under its
  # floor, and the additive assertion below goes red with it. Measured both ways.
  printf '// asserted in %s\n// %s and nowhere else\n// and see the note above/\n// %s is the entry point\n' \
    "$gone_rs_dir" "$gone_rs_tail" "$a_rs" > "$probe/wrapped.ts"
  CITE_SOURCE=unwrap_comment_continuations
  st_case selftest_unwrap_join 2 refuse "$probe/wrapped.ts" "a citation wrapped across two comment lines"
  unwrap_examined=$cites_examined
  CITE_SOURCE=cat
  st_case selftest_unwrap_blind 1 accept "$probe/wrapped.ts" "control: the same file read line by line"
  cat_examined=$cites_examined
  if [ "$unwrap_examined" -gt "$cat_examined" ]; then
    st_ok "the pre-pass is additive: $unwrap_examined citations joined against $cat_examined raw"
  else
    st_bad "the pre-pass added nothing ($unwrap_examined against $cat_examined): either it stopped
       joining, or it REPLACED the raw text and is hiding citations that resolve"
  fi

  gate_arms_finish || FAILED=1
  if [ "$FAILED" -ne 0 ]; then
    echo "::error::doc citation gate SELF-TEST FAILED" >&2
    echo "::error::  The instrument does not discriminate; its clean runs say nothing." >&2
    exit 1
  fi
  echo "doc citation gate self-test: every planted citation refused, every control clean"
  exit 0
fi

# ---------------------------------------------------------------------------
# Arm 1b - AGENTS.md's `path:LINE` citations.
#
# ARM 1 DISCARDS THE LINE NUMBER. Its extractor's character class
# `[A-Za-z0-9_./{},*-]` contains no `:`, so `identity.rs:265-267` is truncated to
# `identity.rs` and only the path is ruled on. AGENTS.md was also in none of the
# `check_citations` calls. So the file every agent loads first got NO line check
# of any kind - not even the past-EOF one every other live document gets.
#
# BE HONEST ABOUT WHAT THIS BUYS. It is four lines and it would have caught NONE
# of the seven wrong citations corrected in AGENTS.md's schema-epoch paragraph on
# 2026-09-04: all seven named a real file and a line INSIDE it, they just named
# the wrong line. Do not read this arm as closing that hole - if the next drift
# is met with "but we gated that", this arm has done net harm. The header above
# explains why the arm that WOULD close it was measured and rejected, twice.
#
# WHAT IS BOUND, AND BY WHAT. Added 2026-09-04, after this arm's own number was
# measured against the capability it exists for. THE COUNT CANNOT SEE THE
# CAPABILITY: delete the line group from the extractor and the root filter -
# which removes every line check this gate performs, on every document - and this
# arm falls 77 -> 76 and stays GREEN at floor 30, because only 7 of the 77 carry
# a line at all. The floor was NOT raised to 7; a floor sized to the thing it
# protects moves whenever somebody edits AGENTS.md, and this file's own history
# records an earlier draft getting that wrong in the other direction.
#
# The past-EOF check is bound instead by `--self-test` above, which plants a
# citation at eof+1, requires a refusal, and puts the eof citation beside it as
# the control. That binds the INSTRUMENT rather than a proxy count, and CI runs
# it as its own step. Under the mutation described, the self-test fails on
# exactly those two planted cases while its eight other cases stay green.
#
# AND THE LINE GROUP IS THE ONLY COLLAPSE NO ARM SEES, re-measured rather than
# assumed. Dropping `crates` from the root filter, or `rs` from the extension
# alternation, leaves THIS arm green at 58 - but turns the whole gate RED through
# arm 7, which falls 28 -> 2 against a floor of 12. So the floors are not useless
# here; they are blind to exactly one thing, which is a capability that touches a
# small minority of every arm's population. That is the shape a self-test is for.
#
# WHAT IS STILL NOT BOUND, and it is the hole the paragraph above describes: a
# citation naming a REAL line that says something else. All seven wrong citations
# were of that kind. No fixture here catches one, none is proposed, and the file
# header records why the two candidate arms were measured and rejected twice.
#
# COUNT AND FLOOR. Measured 2026-09-04: 77 citations, of which SEVEN carry a
# line. The seven are the genuinely new coverage; the other 70 are paths, which
# arm 1 also rules on under a different predicate (`-e`, and a wider root and
# brace-form set) - overlap, not duplication.
#
# The arm is named for what it COUNTS, which is all 77. An earlier draft called
# it `agents_md_line_citations` and gave it a floor of 3, having sized the floor
# against the seven while the number it declared was 77 - a floor a one-step
# collapse clears twenty times over, which is the exact defect found and fixed
# elsewhere in this tree today. 30 is in family with the ratios arms 2b, 6 and 3
# already use (205/40, 170/50, 589/200).
# ---------------------------------------------------------------------------
check_citations AGENTS.md
gate_arm agents_md_citations "$cites_examined" 30 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
agents_line_cites=$cites_examined

check_citations docs/proposals/2026-08-26-*.md \
                docs/proposals/2026-08-31-*.md \
                docs/proposals/2026-07-10-migrate-*.md \
                docs/proposals/2026-07-11-migrate-*.md \
                docs/proposals/2026-07-12-zero-migrate-redesign-plan.md
gate_arm proposal_citations "$cites_examined" 60 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
proposal_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 2b - the LIVE DESIGN SET, and the reason this arm exists is a failure of
# exactly the kind this gate is for.
#
# Arm 2 globs `docs/proposals/2026-08-26-*.md`. The two documents under active
# revision are `docs/proposals/2026-08-28-app-database-decoupling.md` and
# `docs/architecture/data-system.md`. The date glob excludes the first and no
# arm scanned `docs/architecture/` at all, so BOTH WERE INVISIBLE TO THIS GATE.
#
# Measured 2026-08-29: across a session that edited those two files repeatedly,
# every run printed "all resolve" and none of it was about them. The counts that
# moved belonged to the decision log, which does match the 08-26 glob - so the
# gate looked responsive while examining none of the work. That is this gate's
# own founding failure, one directory over: a green that is silent about the
# thing you were changing.
#
# The lesson generalises past these two files. A date-prefixed glob silently
# stops covering a document set the day someone writes tomorrow's date, and
# nothing announces it. If a third design document appears, it must be added
# here or it is unexamined; the floor below is the only thing that will notice
# a file dropping OUT.
#
# THAT WARNING CAME TRUE AND WAS NOT ACTED ON. Measured 2026-09-03: arm 2's
# `2026-08-26-*` and this arm's `2026-08-28-*` between them left SEVEN proposals
# unexamined by any arm - `2026-08-31-data-crate-shape.md`, the design document
# for the crate split then under active revision, and the six `2026-07-*` migrate
# proposals. All seven were rewritten that day and the gate printed a clean green
# about none of them. Arm 2 now names them.
#
# SWEEPING IN `docs/proposals/*.md` WAS TRIED AND REJECTED, and the measurement
# is the reason to leave it rejected: the full glob takes the examined count from
# 303 to 1292 and surfaces 105 dead citations across 24 OTHER proposals. Those
# are not rot this gate should fail on today - per the scope policy at the top of
# this file, a document joins the gate when it has been deliberately CLEANED and
# added, and `docs/decisions/` and `docs/archive/` are exempt outright because a
# dead path in a historical record may be correct. Adding 24 uncleaned documents
# at once would either wedge the gate red or force 105 repairs nobody scoped.
# Clean a proposal, then add it by name on the same commit.
# ---------------------------------------------------------------------------
check_citations docs/architecture/data-system.md \
                docs/proposals/2026-08-28-*.md
gate_arm design_set_citations "$cites_examined" 40 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
design_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 3 - the live feature inventory.
# ---------------------------------------------------------------------------
check_citations docs/feature-map.md
gate_arm feature_map_citations "$cites_examined" 200 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
feature_map_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 4 - operational runbooks.
# ---------------------------------------------------------------------------
check_citations docs/runbooks/*.md
gate_arm runbook_citations "$cites_examined" 20 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
runbook_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 5 - the primary creator build-and-deploy path.
# ---------------------------------------------------------------------------
check_citations docs/build-and-deploy-golden-path.md
gate_arm golden_path_citations "$cites_examined" 5 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
golden_path_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 6 - docs/reference, the stable-contract set AGENTS.md sends readers to.
# It reached zero broken paths on 2026-08-29 and nothing was stopping it drifting
# back; every one of its 15 citations had pointed into `third_party/zero-migrate/`
# for as long as the engine had been in-sourced out of it. The widened extractor
# now rules on bare paths and document/schema citations too, so its floor rises
# with that materially larger population while retaining ample deletion room.
# ---------------------------------------------------------------------------
check_citations docs/reference/*.md
gate_arm reference_citations "$cites_examined" 50 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
reference_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 7 - db/migrations-ts/ comment prose.
#
# WHY A MIGRATION'S COMMENTS ARE A LIVE DOCUMENT. These files carry the
# reasoning for privilege boundaries, and they cite the code that ENFORCES those
# boundaries. `20260818000200_worker_database_authority.ts` said outright "That
# is asserted, not assumed:" and named a test - which had been deleted, so the
# security property was unproven and the file said the opposite. That is a
# stronger failure than a dead path in a design note: the reader is being told
# evidence exists.
#
# `.ts` WAS OUTSIDE EVERY ARM OF THIS GATE. `db` is already in check_citations'
# root allowlist and `ts` already in its extension alternation, so the arm is a
# call, not a new scanner - the gap was scope, not capability.
#
# tests/source_citation_gate.sh DOES scan this directory (its ROOTS) and DID
# catch two of the three; both are ALLOW rows there with a written rationale.
# What neither gate could see was the WRAPPED one, which is what the
# CITE_SOURCE pre-pass above is for.
#
# EDITING THESE FILES IS SAFE, and it was measured rather than assumed before
# the repair that made this arm green. The recorded digest is `Checksum::of_ir`
# over the canonical OP LIST plus flags/owner_app/depends_on/supersedes/
# preconditions (crates/zeroship-migrate-ir/src/migration.rs), and the journal
# key is derived from owner_app and the migration NAME with content deliberately
# excluded (crates/zeroship-migrate-core/src/render/lower.rs). A `//` comment
# produces no op. Driving the real recorder over this file, 2026-09-04: a
# comment edit left the op list byte-identical, while the CONTROL - one `reason:`
# string inside a `raw({...})` op value - moved it. So comment repairs here are
# free; an edit to any op VALUE, or to the exported `name:`, is not.
# ---------------------------------------------------------------------------
CITE_SOURCE=unwrap_comment_continuations
check_citations db/migrations-ts/*.ts
CITE_SOURCE=cat
# FLOOR. Measured 2026-09-04 across 37 files by THIS gate, not by a scratch
# reimplementation: 28 citations. On the committed tree the additive pre-pass
# adds nothing, because the one wrapped citation was repaired in the same commit
# that added this arm - so its value here is prospective, and it is proven by
# mutation rather than by a number: re-wrap any citation and the arm goes red;
# point CITE_SOURCE back at plain `cat` with that same file in place and it goes
# GREEN, blind again.
#
# 12 is a little under half. It has to survive deleting a migration or two (each
# carries 0-3 citations) while catching the collapse that matters: the extractor
# ceasing to match, the glob going empty, or CITE_SOURCE being left pointing at
# something that emits nothing. Deliberately NOT set near 28 - a floor a
# one-step collapse can clear is not a floor, and this tree found exactly that
# defect today in another gate, where a floor of 25 against a population of 77
# was cleared by a collapse to 27.
gate_arm migration_prose_citations "$cites_examined" 12 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
migration_cites=$cites_examined

gate_arms_finish || FAILED=1

if [ "$FAILED" -ne 0 ]; then
  echo "::error::doc citation gate FAILED" >&2
  echo "::error::  A path in prose is a claim. Fix the path, or say DELETED." >&2
  exit 1
fi

echo "doc citations: $agents_examined AGENTS.md paths, $agents_line_cites AGENTS.md citations, $proposal_cites proposal citations, $design_cites design-set citations, $feature_map_cites feature-map citations, $runbook_cites runbook citations, $golden_path_cites golden-path citations, $reference_cites reference citations, $migration_cites migration-prose citations, all resolve"
