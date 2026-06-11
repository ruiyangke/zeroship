# docs/ — navigation aid

Browse this tree by purpose. The repo-root `AGENTS.md` carries the
task-router (per-feature "where to start"). This page only describes
what each top-level subdirectory holds.

| Directory | What lives here |
| --- | --- |
| `architecture/` | Long-form architecture write-ups (one file per major subsystem). Stable. Update when the shape of the system changes. |
| `archive/` | Frozen documents kept for history: shipped proposals, completed design specs/plans, resolved review reports, superseded perf snapshots and benchmark dumps. Mirrors the original subdirectory layout (`archive/perf/`, `archive/benchmarks/`, `archive/reviews/`, `archive/briefs/`, `archive/superpowers/`). Nothing in here is load-bearing; the living surface is `decisions/` + `reference/`. |
| `decisions/` | ADRs — date-prefixed, immutable once landed (`YYYY-MM-DD-topic.md`). The entry surface for "what shipped, when, and why" — each ADR is a short pointer plus link to its long-form design doc (usually now in `archive/`). |
| `design/` | Design-system process docs (how UI work flows from brief to shipped component). |
| `proposals/` | Long-form design docs that are still pre-ship or actively guiding work. Each carries an explicit **Status** line at the top. Shipped proposals move to `archive/`; `TRIAGE.md` is the standing triage worklist. |
| `reference/` | Stable user-facing contracts (db, kv, rpc, auth, billing, websocket, plugin-system, …). The "API surface" of the platform. |
| `research/` | Competitive landscape and ecosystem research. |
| `reviews/` | Active review reports only (in-flight audits, the freshest security review). Resolved reviews move to `archive/reviews/`. |
| `runbooks/` | Operational how-tos: local dev, multi-node Compose, DB migrations, auth deploy, sandbox backends, private registry, k3s/crun/krun host. |
| `superpowers/` | Specs and plans still referenced by live code or docs (builder design spec, console-app specs, auth-server phase plans). Completed ones move to `archive/superpowers/`. |

Dated perf investigations and cross-runtime benchmark dumps live under
`archive/perf/` and `archive/benchmarks/`; new investigations should be
committed straight to those archive paths once closed (there is no
top-level `perf/` anymore).
