
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
