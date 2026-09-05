# live_docs.sh - the ONE list of documents this repo treats as making live,
# checkable claims. Sourced by tests/doc_citation_gate.sh (which rules on the
# PATHS they cite) and tests/cargo_package_spec_gate.sh (which rules on the
# CARGO SELECTORS in their fenced command blocks).
#
# WHY THIS IS A FILE AND NOT TWO LISTS. Two gates each carrying a hand-maintained
# document set is a census in two places, and this repo has been burned by
# censuses. doc_citation_gate.sh's own arm-2b header records the failure exactly:
# a date-prefixed glob quietly stopped covering the two documents under active
# revision, every run printed a clean green, and none of it was about them. A
# second copy of that list in another gate would drift the same way, except the
# drift would be silent in a DIFFERENT gate and nobody would think to compare.
#
# WHAT "LIVE" MEANS, and it is a policy not an inventory. A document joins this
# list when it has been deliberately CLEANED and added on the same commit -
# never by widening a glob. `docs/decisions/` and `docs/archive/` are EXEMPT by
# policy and must never appear here: they are historical records, so a dead path
# in one may be correct. `docs/proposals/*.md` as a whole is deliberately NOT
# here - sweeping it in was measured on 2026-09-03 and rejected, because it
# surfaces 105 dead citations across 24 uncleaned proposals.
#
# ADDING A DOCUMENT IS A REAL COMMITMENT: both gates start ruling on it at once.

# Expands to the live document set. Globs are expanded by the caller's shell at
# call time, so a file added to docs/reference/ is covered without editing this.
live_docs() {
  printf '%s\n' \
    AGENTS.md \
    docs/architecture/data-system.md \
    docs/feature-map.md \
    docs/build-and-deploy-golden-path.md \
    docs/reference/*.md \
    docs/runbooks/*.md \
    docs/proposals/2026-08-26-*.md \
    docs/proposals/2026-08-28-*.md \
    docs/proposals/2026-08-31-*.md \
    docs/proposals/2026-07-10-migrate-*.md \
    docs/proposals/2026-07-11-migrate-*.md \
    docs/proposals/2026-07-12-zero-migrate-redesign-plan.md \
    | while IFS= read -r d; do [ -f "$d" ] && printf '%s\n' "$d"; done
}
