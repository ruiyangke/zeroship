// Typed RPC client for the builder's streaming chat surfaces. Unary
// procedures are direct imports from server modules; the vite-plugin
// turns those imports into RPC stubs. Streams use explicit per-procedure
// handles so `streamUrl()` stays available for AI SDK transports.
//
// `rpc.chat.streamUrl()` is the AI-SDK entry point — `@zeroship/rpc`
// exposes it explicitly so we can hand the URL to `useChat` without
// hardcoding `/_zs/v1/chat`. The transport-level body envelope
// (`{ json: <input> }`) is wrapped in `chatTransport` below.

import { createRpcClient } from "@zeroship/rpc/client";
import { DefaultChatTransport } from "ai";
import type { UIMessage } from "ai";

// Compatibility shim for the orphan tree (auth pages, admin, old
// project workspace). Vite resolves bare `from "../api"` to api.ts
// rather than api/index.ts, so the old api/index.ts is dead code.
// TopBar / AuthContext / Account / Login / Signup still reference
// `isDevAutoAuth` via that import path; export the same shim here
// so the module chain loads. A later cleanup can delete this once the
// orphaned surfaces are removed.
export function isDevAutoAuth(): boolean {
  return import.meta.env.DEV;
}

// ─── Project Lifecycle Procedures ─────────────────────────────────
// Re-export server functions as plain async functions. The vite-plugin
// transforms each server-module export into an RPC stub on the client
// side (see sdks/vite-plugin/src/transform.ts:317-329 — single-input
// wire). Callers `import { createApp } from "../api"` and call them
// like normal async functions; the wire is `/_zs/v1/apps.<name>`.
//
// Note on positional args: the wire forwards `args[0]` only. Every
// proc that needs more than one input takes a single object input
// (e.g. `createApp({ name, plan_id })`).
export {
  listApps,
  getApp,
  createApp,
  deleteApp,
  deployApp,
  updatePlan,
  getAppLogs,
  listVars,
  setVar,
  deleteVar,
  listSecrets,
  setSecret,
  deleteSecret,
  archiveApp,
  unarchiveApp,
  type AppRecord,
  type EnvVar,
} from "../server/apps";

// Sandbox file procs used by the FilesCanvas. Both take object input
// so the single-input RPC wire delivers every field.
export {
  listSandboxFiles,
  readSandboxFile,
  type FileEntry,
} from "../server/sandbox";

// Plan / Health canvas stubs. Every export takes one
// object input — see `server/agents.ts` header for the wire-shape
// rationale. The current implementation is in-memory only.
//
// Data + Media canvas stubs live in the same module with the same
// single-input wire convention. Real persistence and backing stores
// are still separate follow-up work.
export {
  listIssues,
  addIssue,
  getQualityScores,
  // Data canvas
  listTables,
  getTableRows,
  listIndexes,
  listMigrations,
  listBackups,
  triggerBackup,
  // Media canvas
  listMedia,
  uploadMedia,
  deleteMedia,
  type Issue,
  type IssueStatus,
  type IssueSource,
  type QualityScores,
  type QualityDimension,
  type QualityGrade,
  type TableSummary,
  type TableRow,
  type IndexInfo,
  type MigrationEntry,
  type MigrationStatus,
  type BackupEntry,
  type BackupKind,
  type MediaEntry,
} from "../server/agents";

// PM / SRE scheduled-worker procs (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.3.2). Each takes
// `{appId}` and returns a structured digest / findings payload — see
// `server/pm_worker.ts` and `server/sre_worker.ts` for the wire shape
// and the missing cron infrastructure caveat.
export {
  pmDigest,
  type PMDigest,
  type PMDigestRecommendationItem,
  type PMDigestInput,
} from "../server/pm_worker";
export {
  sreMonitor,
  type SREMonitorResult,
  type SREFindingItem,
  type SREMonitorInput,
} from "../server/sre_worker";

export { appPreviewUrl } from "./lib/preview-url";

export const rpc = {
  chat: createRpcClient().stream<{ messages: UIMessage[]; appId?: string }, never>("chat"),
  // Wizard: project-creation flow runtime, plain LangGraph (see
  // §4.8.2b). Same wire envelope as chat, but the input is `{idea, id}`
  // on a fresh turn (no message history — the wizard's checkpointer
  // owns state) and `{resume, id}` on a SurveyCard submit.
  wizard: createRpcClient().stream<
    { idea?: string; id?: string; resume?: { token: string; value: unknown }; messages?: UIMessage[] },
    never
  >("wizard"),
};

/**
 * AI SDK transport bound to a streaming RPC procedure. Wraps the
 * `useChat` request shape in zeroship's superjson envelope
 * (`{ json: ... }`) and points at the procedure's stream URL.
 *
 * Two send shapes flow through this transport:
 *   1. Normal turn — `sendMessage({ text })` from a composer:
 *      wire body = `{ json: { messages: UIMessage[], id } }`.
 *   2. Resume turn — `sendMessage(_, { body: { resume: {token,value} }})`
 *      after a SurveyCard submit (server tool halted via
 *      `interrupt()`): wire body = `{ json: { resume, id } }`. Messages
 *      are stripped because the server feeds `Command({resume})` into
 *      the existing thread instead of replaying history.
 *
 * Usage:
 *   const { messages, sendMessage } = useChat({
 *     transport: chatTransport(rpc.chat),
 *   });
 */
export function chatTransport<TIn>(
  handle: {
    streamUrl: (input?: TIn) => string | Promise<string>;
  },
  options: { appId?: string } = {},
) {
  return new DefaultChatTransport({
    // streamUrl() with no input returns a synchronous URL
    // (`/_zs/v1/<id>`). With `transformer: "superjson"` set on the
    // client it would return a Promise — fine for `api`, which
    // DefaultChatTransport accepts as either form.
    api: handle.streamUrl() as string,
    prepareSendMessagesRequest: ({ messages, id, body }) => {
      // Resume payload from a SurveyCard (or any future interrupt).
      // ChatRail attaches it via `sendMessage(_, { body: { resume } })`.
      // The server's chat.ts treats `body.json.resume` as the cue to
      // skip message replay and feed Command({resume}) into the same
      // thread.
      const resume = (body as { resume?: { token: string; value: unknown } } | undefined)?.resume;
      // appId carried alongside both fresh and resume payloads so the
      // server's data-part middleware can persist Critic-graded
      // scorecards back to the right project.
      // Optional — when missing (rare; tests, default thread), the
      // middleware skips the persistence side-effect.
      const appId = options.appId;
      if (resume) {
        return { body: { json: { resume, id, appId } } };
      }
      return { body: { json: { messages, id, appId } } };
    },
  });
}

/**
 * AI SDK transport bound to the wizard streaming procedure. Same
 * envelope as chatTransport, different input shape on a fresh turn:
 * the wizard server expects `{ idea, id }` (no `messages`), so we
 * extract the user's typed text from the first message rather than
 * shipping the full UIMessage[] history.
 *
 *   Fresh:   `sendMessage({ text })`           → wire `{ idea, id }`
 *   Resume:  `sendMessage(_, { body:{resume}})` → wire `{ resume, id }`
 *
 * The wizard's checkpointer (keyed by `id`) owns conversation state;
 * messages don't need to round-trip on subsequent fresh turns. In
 * practice "subsequent fresh turns" don't happen anyway — once the
 * wizard halts on a survey, every continuation is a resume until the
 * terminal data-brief arrives and the surface ends.
 */
export function wizardTransport<TIn>(handle: {
  streamUrl: (input?: TIn) => string | Promise<string>;
}) {
  return new DefaultChatTransport({
    api: handle.streamUrl() as string,
    prepareSendMessagesRequest: ({ messages, id, body }) => {
      const resume = (body as { resume?: { token: string; value: unknown } } | undefined)?.resume;
      if (resume) {
        return { body: { json: { resume, id } } };
      }
      // Extract the idea from the latest user message (typically the
      // only user message — the wizard surface composer disables after
      // the first send). Defensive: empty / missing falls through as
      // empty string and the wizard's "decide" node will produce a
      // generic survey or finalize.
      const lastUser = [...(messages ?? [])]
        .reverse()
        .find((m) => m.role === "user");
      const idea = lastUser
        ? (lastUser.parts as Array<{ type: string; text?: string }>)
            .filter((p) => p.type === "text")
            .map((p) => p.text ?? "")
            .join("")
        : "";
      return { body: { json: { idea, id } } };
    },
  });
}
