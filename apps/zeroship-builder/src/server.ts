"use server";
// Server entry — discovered by `@zeroship/vite-plugin` (which looks
// for `src/server.ts` by convention). Re-exports every server-function
// module so the plugin's transform can register them all in one
// bundled fetch handler.

export * from "./server/projects";
export * from "./server/sandbox";
export * from "./server/chat";
export * from "./server/wizard";
export * from "./server/agents";
export { builderFetch as fetch } from "./server/http";
// PM/SRE scheduled-worker procs (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.3.2 dual-shape stubs).
// Both expose a single proc each (`pmDigest`, `sreMonitor`) — kept in
// dedicated modules rather than folded into agents.ts so the chat-mode
// SubAgent files (`internal/pm.ts` / `internal/sre.ts`) and the worker files cluster
// by name and the import surface stays scannable. Cron infrastructure
// to drive them is not wired yet.
export * from "./server/pm-worker";
export * from "./server/sre-worker";
