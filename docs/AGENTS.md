# docs/ — navigation aid

Browse this tree by purpose. The repo-root `AGENTS.md` carries the
task-router (per-feature "where to start"). This page only describes
what each top-level subdirectory holds.

| Directory | What lives here |
| --- | --- |
| `architecture/` | Long-form architecture write-ups (one file per major subsystem). Stable. Update when the shape of the system changes. |
| `archive/` | Frozen documents kept for history: superseded perf snapshots, completed review-loop artifacts, design critiques whose findings have shipped. Mirrors the original subdirectory layout (`archive/perf/`, `archive/reviews/`, `archive/proposals/`). |
| `benchmarks/` | Cross-runtime benchmark output dumps (txt). One file per session. |
| `decisions/` | ADRs — date-prefixed, immutable once landed (`YYYY-MM-DD-topic.md`). |
| `perf/` | Active performance investigations: regression notes, time-distribution decompositions, microbenches, flamegraphs (`perf/flamegraphs/*.svg`). Snapshots are dated and superseded files move to `archive/perf/`. |
| `proposals/` | Pre-ship design docs and shipped specs that have no equivalent reference doc. Each carries an explicit **Status** line at the top (Draft / In progress / Shipped / Partially shipped). |
| `reference/` | Stable user-facing contracts (db, auth, billing, websocket, plugin-system, …). The "API surface" of the platform. |
| `research/` | Competitive landscape and ecosystem research. |
| `reviews/` | Active critique reports for in-flight design work. Completed review loops move to `archive/reviews/<topic>/`. |
| `runbooks/` | Operational how-tos: local dev, multi-node Compose, sandbox backends, k3s/crun/krun host. |
| `superpowers/` | Builder skill definitions and plans (`specs/`, `plans/`) plus zeroship-builder branch artifacts (status, spec-compliance, accessibility audit). |
