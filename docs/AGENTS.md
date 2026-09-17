# docs/ — navigation aid

The repo-root `AGENTS.md` carries the principles; this page says what each
subdirectory holds. The living surface is `decisions/` and `reference/`.

| Directory | What lives here |
| --- | --- |
| `architecture/` | Long-form architecture write-ups, one file per major subsystem. Stable; update when the shape of the system changes. |
| `decisions/` | ADRs — date-prefixed, immutable once landed (`YYYY-MM-DD-topic.md`). The entry surface for what shipped, when, and why. |
| `proposals/` | Long-form design docs that are still pre-ship or actively guiding work. Each carries an explicit **Status** line at the top. |
| `reference/` | Stable creator-facing contracts (db, kv, rpc, auth, billing, storage, websocket, …). The platform's API surface. |
| `runbooks/` | Operational how-tos: local dev, multi-node Compose, DB migrations, auth deploy, private registry, host setup. |

## `reference/` is the creator-facing contract

Everything under it is written for the person building an app: what they can rely on,
the surfaces they call, the defaults, the limits, and the errors they will see.

It carries **no implementation detail and no code references** — no module paths, crate
names, symbol names or line numbers. Error *codes* and wire shapes are creator-facing
and belong there; how the platform makes them true belongs in `architecture/`, or in
the code itself.
