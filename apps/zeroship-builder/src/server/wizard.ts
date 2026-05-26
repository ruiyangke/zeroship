"use server";
// Wizard RPC procedure — `/_zs/v1/wizard`. The pre-coding clarification
// flow per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.2b + §8.2.7. Plain-LangGraph backend (NOT
// deepagents) — see the body below for why.
//
// Wire (mirrors chat.ts so the client transport is reusable):
//   POST /_zs/v1/wizard
//     fresh body:   { json: { idea: string, id: string } }
//     resume body:  { json: { resume: { token, value }, id: string } }
//     response:     text/event-stream  (UI Message Stream)
//                   chunks: data-survey* + data-brief (terminal)
//
// The wizard's stream contains ONLY data-* chunks — no text deltas, no
// tool-call chunks. Everything user-visible is rendered from
// data-survey (SurveyCard) and the eventual data-brief (a Begin button
// / brief preview, future client work).
//
// --- Runtime: plain LangGraph (NO deepagents, NO sandbox) ---------------
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.2b: the project-creation wizard runs *before* a
// project exists. It has no fs/exec/SubAgent/todo/summarisation
// requirements (cost test: 0/5), so loading deepagents and
// provisioning a sandbox per visitor would be pure overhead. We use
// plain LangGraph instead — a small StateGraph that loops surveys
// against the LLM until the brief is complete.
//
// Wire compatibility: the wizard emits the SAME `data-survey` chunk
// shape as Builder (§8.2.7) and uses the same resume protocol.
// Client-side <SurveyCard> renders identically; client-side
// `chatTransport.prepareSendMessagesRequest` already routes resume
// payloads. The only client-visible difference is the RPC procedure
// (/_zs/v1/wizard vs /_zs/v1/chat) and the terminal `data-brief` chunk
// the wizard emits when the brief is complete (Builder doesn't
// produce briefs; it consumes them).
//
// ---
//
// **Two-node architecture, NOT one.** The naive shape is one node
// that calls model.invoke() and then interrupt() based on the
// decision. That used to fail in the zeroship V8 isolate: native
// fetch broke AsyncLocalStorage propagation, so by the time
// `interrupt()` ran after `await model.invoke()`, langgraph's
// `getRunnableConfig()` returned null and we got
//
//     "Called interrupt() outside the context of a graph."
//
// **Status as of 2026-05-04:** the underlying runtime bug is closed.
// `crates/runtime/src/node/async_hooks/` now ships a native
// AsyncLocalStorage backed by V8's
// `ContinuationPreservedEmbedderData`, which V8 propagates across
// every async hop including continuations resumed from native
// `fetch`. The two-node shape below is therefore no longer
// REQUIRED — `interrupt()` after `await model.invoke()` works in a
// single node now. Collapsing this back into a single node is left
// to a follow-up PR (functional behaviour is identical; the split
// is only a code-shape difference).
//
// Fix (kept in place for now): split into two nodes.
//   - `decide` — calls model.invoke, stashes decision in state. No
//     interrupt() in this node.
//   - `act`   — reads the stashed decision; if ask_survey, emits
//     data-survey then interrupt(); if finalize, emits data-brief
//     and ends.
//
// Routing: START → decide → act → (loop or END). The "loop" path
// goes act → decide so the next round fetches a fresh decision based
// on the answer just collected.
//
// ---
//
// Lifecycle: a wizard "session" lives only until Begin. The brief is
// stashed by the client (sessionStorage / route state) and seeded
// into Builder's first turn. If the user navigates away mid-wizard,
// no DB rows or sandboxes leak (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.4 runtime-handoff
// section).

import { createUIMessageStream, createUIMessageStreamResponse, type UIMessage, type UIMessageStreamWriter } from "ai";
import { stream as rpcStream } from "@zeroship/rpc/server";
import { z } from "zod";

import { emitDataSurvey, surveyInputSchema, type SurveyInput } from "./internal/survey-wire.js";
import { WIZARD_SYSTEM } from "./internal/prompts.js";

// --- Wire input ---------------------------------------------------------

export interface WizardTurnInput {
  /**
   * Free-text idea from the home prompt or /new textarea. Required
   * on the first turn; ignored on resume turns (the LLM already has
   * the idea via checkpointer state).
   */
  idea?: string;
  /**
   * Wizard session id — becomes the LangGraph thread_id so the
   * checkpointer scopes accumulated brief/history to this session.
   * Use a fresh UUID per "new project" attempt; when the client
   * navigates away the id is dropped and the in-memory checkpointer
   * entry GCs.
   */
  id?: string;
  /**
   * Resume payload — present iff client is answering a SurveyCard.
   * Same shape as Builder's chat resume.
   */
  resume?: { token: string; value: unknown };
  /**
   * Optional message history (for parity with `useChat`'s default
   * body shape). The wizard ignores this — its own state lives in
   * the checkpointer keyed by `id`. Only `idea` and `resume` are
   * load-bearing inputs.
   */
  messages?: UIMessage[];
}

// --- Brief shape (output) -----------------------------------------------

export interface WizardBrief {
  idea: string;
  summary: string;
  answers: Array<{ question: string; answer: unknown }>;
}

// --- Internal state -----------------------------------------------------

let _wizardCheckpointer: import("@langchain/langgraph-checkpoint").BaseCheckpointSaver | null = null;
async function getWizardCheckpointer(): Promise<
  import("@langchain/langgraph-checkpoint").BaseCheckpointSaver
> {
  if (_wizardCheckpointer) return _wizardCheckpointer;
  const { MemorySaver } = await import("@langchain/langgraph");
  _wizardCheckpointer = new MemorySaver();
  return _wizardCheckpointer;
}

const DEFAULT_THREAD_ID = "wizard-default";

// Cap surveys per session to bound runaway loops. Each round is a
// real LLM call + a real user wait; >5 is friction territory. The
// LLM is also told this in WIZARD_SYSTEM, but enforcing here is the
// safety net.
const MAX_SURVEY_ROUNDS = 5;

// LLM output: a flat object with a `kind` discriminator + optional
// per-branch payloads. We don't use Zod's discriminatedUnion at the
// top level because OpenAI's structured-output APIs translate it to
// `anyOf` (no top-level `type: "object"`), which both jsonSchema and
// functionCalling modes reject. A single object with conditionally-
// populated fields validates cleanly and we discriminate at runtime.
const wizardDecisionSchema = z.object({
  kind: z.enum(["ask_survey", "finalize"]).describe(
    "ask_survey to halt and ask the user; finalize to produce the final brief and end.",
  ),
  survey: surveyInputSchema.optional().describe(
    "Required when kind=ask_survey. The survey to show the user.",
  ),
  summary: z
    .string()
    .optional()
    .describe(
      "Required when kind=finalize. A 2-3 sentence concrete summary of what the user wants to build, ready to hand to Builder.",
    ),
});
type WizardDecision = z.infer<typeof wizardDecisionSchema>;

// --- The stream builder -------------------------------------------------

async function buildWizardStream(
  input: WizardTurnInput,
  signal?: AbortSignal,
) {
  const { ChatOpenAI } = await import("@langchain/openai");
  const {
    StateGraph,
    Annotation,
    START,
    END,
    Command,
    isGraphInterrupt,
    interrupt,
  } = await import("@langchain/langgraph");

  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    throw new Error(
      "OPENAI_API_KEY is not set. Configure it in apps/zeroship-builder/.env.",
    );
  }

  // Same model family as Builder. `functionCalling` mode (vs default
  // jsonSchema strict mode) tolerates `.optional()`
  // fields without forcing them all to `.nullable()`. The shared
  // surveyInputSchema (used by Builder too) uses `.optional()` for
  // preamble / skip_label / placeholder etc.; with strict mode the
  // OpenAI API rejects the request: "uses `.optional()` without
  // `.nullable()` which is not supported".
  const model = new ChatOpenAI({
    model: "gpt-5.4-mini",
    temperature: 0.3,
    apiKey,
  }).withStructuredOutput(wizardDecisionSchema, {
    name: "wizard_decision",
    method: "functionCalling",
  });

  const threadId = input.id ?? DEFAULT_THREAD_ID;
  const isResume = Boolean(input.resume);
  const checkpointer = await getWizardCheckpointer();

  return createUIMessageStream({
    async execute({ writer }) {
      // State — split into "user-visible content" (idea, answers,
      // summary, rounds, done) and "graph-internal scratch"
      // (pendingDecision: the model's last decision, set by `decide`,
      // consumed by `act`). The scratch field is reset to null after
      // act consumes it so a re-entry to decide isn't confused by
      // stale state.
      const WizardState = Annotation.Root({
        idea: Annotation<string>({
          reducer: (_prev, next) => next,
          default: () => "",
        }),
        answers: Annotation<Array<{ question: string; answer: unknown }>>({
          reducer: (prev, next) => [...prev, ...next],
          default: () => [],
        }),
        summary: Annotation<string>({
          reducer: (_prev, next) => next,
          default: () => "",
        }),
        rounds: Annotation<number>({
          reducer: (prev, next) => prev + next,
          default: () => 0,
        }),
        done: Annotation<boolean>({
          reducer: (_prev, next) => next,
          default: () => false,
        }),
        pendingDecision: Annotation<WizardDecision | null>({
          reducer: (_prev, next) => next,
          default: () => null,
        }),
      });

      // Node 1: decide. Calls the model. NO interrupt here — the
      // model.invoke await breaks AsyncLocalStorage in zeroship's V8,
      // so any interrupt() call after it would fail. We just stash
      // the decision and return.
      const decide = async (state: typeof WizardState.State) => {
        // Safety net: cap rounds. If we've already hit the cap,
        // synthesize a finalize decision so `act` ends gracefully.
        if (state.rounds >= MAX_SURVEY_ROUNDS) {
          const summary = `User wants: ${state.idea}. Captured ${state.answers.length} survey answers.`;
          return {
            pendingDecision: { kind: "finalize" as const, summary },
          };
        }

        const decision = await model.invoke(
          buildDecisionMessages(state),
          { signal },
        );

        return { pendingDecision: decision };
      };

      // Node 2: act. Reads the decision, emits the appropriate UI
      // chunk, halts via interrupt() if it's a survey. NO awaits
      // before interrupt() — AsyncLocalStorage is freshly set by
      // langgraph when this node starts, so interrupt() finds the
      // graph context.
      //
      // Resume re-enters this node from the top (langgraph replays
      // the interrupted node). On resume we skip the data-survey
      // emit — the client already has the card, and re-emitting
      // would render a stale duplicate. We track resume mode via
      // the closure's `isResume` flag (set at request entry, before
      // graph.streamEvents). After the first turn it's false; after
      // a resume turn it's true; we reset it once we've passed the
      // resumed interrupt so subsequent loop iterations (decide →
      // act with a NEW survey) get a fresh emit.
      let suppressNextEmit = isResume;
      const act = (state: typeof WizardState.State) => {
        const decision = state.pendingDecision;
        if (!decision) {
          // Should never happen — decide always sets it. But if it
          // does, end gracefully rather than hang.
          emitDataBrief(writer, {
            idea: state.idea,
            summary: `User wants: ${state.idea}.`,
            answers: state.answers,
          });
          return { done: true, pendingDecision: null };
        }

        if (decision.kind === "finalize") {
          const summary =
            decision.summary && decision.summary.length >= 20
              ? decision.summary
              : `User wants: ${state.idea}.`;
          emitDataBrief(writer, {
            idea: state.idea,
            summary,
            answers: state.answers,
          });
          return { done: true, summary, pendingDecision: null };
        }

        // ask_survey path. Defensive: missing survey → finalize.
        if (!decision.survey) {
          const summary = `User wants: ${state.idea}.`;
          emitDataBrief(writer, { idea: state.idea, summary, answers: state.answers });
          return { done: true, summary, pendingDecision: null };
        }
        const survey = decision.survey;

        // First call (no resume value): emit + interrupt throws.
        // Resume call: skip emit (client already has the card),
        // interrupt returns the resume value, node continues.
        // After this point we've consumed the suppression flag — a
        // subsequent loop iteration that asks ANOTHER survey is a
        // fresh emit again.
        if (!suppressNextEmit) {
          emitDataSurvey(writer, survey);
        }
        suppressNextEmit = false;
        const answer = interrupt({
          kind: "ask_survey",
          survey,
        }) as { skipped?: boolean; answers?: Record<string, unknown> };

        // Resume reaches here. Convert the {answers} payload into
        // (question, answer) pairs so the next decide turn sees them
        // in human-readable form. Bump rounds so MAX_SURVEY_ROUNDS
        // works.
        const newAnswers: Array<{ question: string; answer: unknown }> = [];
        if (!answer?.skipped && answer?.answers) {
          for (const q of survey.questions) {
            if (q.id in answer.answers) {
              newAnswers.push({
                question: q.prompt,
                answer: answer.answers[q.id],
              });
            }
          }
        } else if (answer?.skipped) {
          newAnswers.push({ question: "(survey skipped)", answer: null });
        }

        return { answers: newAnswers, rounds: 1, pendingDecision: null };
      };

      // Routing: START → decide → act → (loop back to decide if
      // more rounds, else END). The conditional edge needs an
      // explicit pathMap (third arg) — without it langgraph can't
      // statically know the destinations and the wiring silently
      // fails (observed: graph runs START → END skipping the
      // configured nodes).
      const graph = new StateGraph(WizardState)
        .addNode("decide", decide)
        .addNode("act", act)
        .addEdge(START, "decide")
        .addEdge("decide", "act")
        .addConditionalEdges(
          "act",
          (state) => (state.done ? END : "decide"),
          ["decide", END],
        )
        .compile({ checkpointer });

      // Input depends on mode. Fresh: seed `idea`. Resume: feed
      // Command({resume:value}) so the interrupted node continues.
      const graphInput = isResume
        ? new Command({ resume: input.resume!.value })
        : { idea: input.idea ?? "" };

      try {
        // streamEvents drains events as the graph runs. We don't
        // forward intermediate events to the client (the nodes
        // already write data-* chunks via the writer); we just need
        // to await completion or the GraphInterrupt halt.
        // streamEvents accepts `UpdateType | CommandInstance | null`, but
        // the Command<unknown, Record<string, unknown>, string> from
        // `new Command({resume})` doesn't unify with the strongly-typed
        // CommandInstance<...> that's narrowed to this graph's state
        // channels — TS can't see that resume-mode Command doesn't update
        // any channels. Cast through `any`, same escape hatch used in
        // _translator.ts. Wire shape is verified at runtime via smoke.
        const events = graph.streamEvents(graphInput as any, {
          version: "v2" as const,
          configurable: { thread_id: threadId },
          signal,
        });
        for await (const _ of events) {
          /* drain */
        }
      } catch (err) {
        if (!isGraphInterrupt(err)) throw err;
        // Halted via interrupt() inside `act` — data-survey already
        // emitted. Stream closes cleanly; client renders SurveyCard,
        // eventually submits via the resume protocol.
      }
    },
  });
}

// --- helpers ------------------------------------------------------------

function emitDataBrief(writer: UIMessageStreamWriter, brief: WizardBrief): void {
  // `data-brief` is wizard-only — Builder doesn't emit it. Client-side
  // a future BriefCard / Begin button reads this chunk and either
  // auto-confirms or lets the user click Begin to commit.
  try {
    writer.write({
      type: "data-brief",
      id: crypto.randomUUID(),
      data: brief,
    } as Parameters<UIMessageStreamWriter["write"]>[0]);
  } catch {
    // Stream closed — caller handles. Brief was the terminal chunk
    // anyway.
  }
}

function buildDecisionMessages(state: { idea: string; answers: Array<{ question: string; answer: unknown }> }) {
  const trail = state.answers.length === 0
    ? "(no answers yet)"
    : state.answers
        .map((a) => `Q: ${a.question}\nA: ${stringifyAnswer(a.answer)}`)
        .join("\n\n");

  return [
    { role: "system" as const, content: WIZARD_SYSTEM },
    {
      role: "user" as const,
      content:
        `User's idea:\n${state.idea}\n\n` +
        `Survey answers so far:\n${trail}\n\n` +
        `Decide: ask one more survey (kind="ask_survey") OR finalize the brief (kind="finalize"). ` +
        `Stop asking once the brief is concrete enough for Builder to start coding.`,
    },
  ];
}

function stringifyAnswer(value: unknown): string {
  if (value == null) return "(skipped)";
  if (typeof value === "string") return value;
  if (typeof value === "boolean") return value ? "yes" : "no";
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

// --- RPC handler --------------------------------------------------------

export const wizard = rpcStream(
  (async (input: WizardTurnInput): Promise<Response> => {
  // Same AbortController-on-stream-cancel pattern as chat.ts. The
  // kernel RPC fast path doesn't expose request.signal, so we mint
  // our own and abort when the response body is cancelled.
    const ac = new AbortController();
    const stream = await buildWizardStream(input, ac.signal);

    const baseResponse = createUIMessageStreamResponse({ stream });
    if (!baseResponse.body) return baseResponse;

    const wrapped = new ReadableStream({
      async start(controller) {
        const reader = baseResponse.body!.getReader();
        try {
          while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            controller.enqueue(value);
          }
          controller.close();
        } catch (err) {
          controller.error(err);
        }
      },
      cancel(reason) {
        ac.abort(reason);
      },
    });

    return new Response(wrapped, {
      status: baseResponse.status,
      statusText: baseResponse.statusText,
      headers: baseResponse.headers,
    });
  }) as unknown as (input: WizardTurnInput) => AsyncIterable<never>,
  { id: "wizard", lazy: true },
) as unknown as (input: WizardTurnInput) => Promise<Response>;
