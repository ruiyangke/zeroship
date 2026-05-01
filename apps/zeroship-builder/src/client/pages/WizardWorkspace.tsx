// Wizard surface — pre-coding clarification chat. Per spec §8.2.7 +
// §4.8.2b: this page talks to the wizard runtime (plain LangGraph,
// `apps/zeroship-builder/src/server/_wizard.ts`), NOT Builder. The
// wizard halts on data-survey chunks; the user answers; once the LLM
// decides the brief is concrete enough it emits a terminal data-brief
// chunk that we render as a BriefCard with a Begin button.
//
// On Begin: per spec §8.2.4, this is where project creation +
// sandbox provisioning happen synchronously and we navigate to
// /p/<id>/preview where the WORKSPACE Builder takes over. We:
//   1. derive a project name from the brief idea (first 3 words),
//   2. POST createApp(name) to the control plane,
//   3. stash the brief in sessionStorage under `zeroship_pending_brief`,
//   4. navigate to /p/<app.id>/preview where WorkspaceShell consumes
//      the stash and seeds ChatRail with a synthesised first message.

import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useChat } from "@ai-sdk/react";
import { useMutation } from "@tanstack/react-query";
import { createApp, rpc, wizardTransport } from "../api";
import { NotebookPrompt, CmdEnterHint } from "../components/NotebookPrompt";
import { Button } from "../components/Button";
import { ChatMessages } from "../workspace/chat/ChatMessages";
import type { Brief, SurveyResponse } from "../types/chat";

const PENDING_BRIEF_KEY = "zeroship_pending_brief";

/**
 * Derive a deterministic project name from a free-text idea. Take the
 * first three whitespace-separated words and Title-Case them; falls
 * back to "Untitled" when the idea is empty/whitespace. Matches the
 * shape control-plane `name` accepts (server-side validation lives in
 * the control plane, not here — if it rejects, the mutation surfaces
 * the error inline).
 */
function deriveProjectName(idea: string): string {
  const words = idea.trim().split(/\s+/).filter(Boolean).slice(0, 3);
  if (words.length === 0) return "Untitled";
  return words
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase())
    .join(" ");
}

// Inline minimal frame instead of PageFrame because the latter pulls
// in TopBar → useAuth → AuthProvider, which lives in the orphan tree
// from the pre-redesign auth pages and has broken imports. Plan 03
// restores AuthProvider at the App root and we'll switch to PageFrame
// then. Keeping the wizard layout decoupled also matches §4.8.2b: the
// wizard runs *before* a project (and arguably before a real auth
// session in some flows), so a lightweight standalone frame fits.

export function WizardWorkspace() {
  // Stable session id for the wizard's checkpointer. One id per
  // mount — refresh = clean wizard. The id is also threaded through
  // the resume protocol (server uses it as LangGraph thread_id).
  const sessionId = useMemo(() => crypto.randomUUID(), []);
  const navigate = useNavigate();

  const [draft, setDraft] = useState("");
  const [committedBrief, setCommittedBrief] = useState<Brief | null>(null);
  const [answeredSurveys, setAnsweredSurveys] = useState<Set<string>>(() => new Set());

  const { messages, sendMessage, status, error } = useChat({
    id: sessionId,
    transport: wizardTransport(rpc.wizard),
    onError: (err) => console.error("[wizard]", err),
  });

  // createApp mutation — `useMutation` gives us isPending + error
  // without manual state. On success we stash the brief and navigate;
  // on failure the error band at the bottom of the page renders it.
  const createMutation = useMutation({
    mutationFn: async (vars: { name: string; brief: Brief }) => {
      const app = await createApp(vars.name);
      return { app, brief: vars.brief };
    },
    onSuccess: ({ app, brief }) => {
      try {
        sessionStorage.setItem(PENDING_BRIEF_KEY, JSON.stringify(brief));
      } catch {
        // Quota / disabled storage — workspace still loads, the chat
        // rail just won't auto-seed. User can paste it manually.
      }
      navigate(`/p/${app.id}/preview`);
    },
  });

  const busy = status === "submitted" || status === "streaming";
  // Disable composer once the wizard has started — the wizard takes
  // over via surveys. The user goes back to typing only when they
  // refresh or navigate away.
  const wizardStarted = messages.length > 0;

  function handleSendIdea() {
    const text = draft.trim();
    if (!text || busy || wizardStarted) return;
    sendMessage({ text });
  }

  function submitSurvey(token: string, response: SurveyResponse) {
    if (busy || answeredSurveys.has(token)) return;
    setAnsweredSurveys((s) => {
      const next = new Set(s);
      next.add(token);
      return next;
    });
    const value = response.skipped
      ? { skipped: true }
      : { skipped: false, answers: response.answers };
    sendMessage(undefined, { body: { resume: { token, value } } });
  }

  function handleBegin(brief: Brief) {
    if (committedBrief || createMutation.isPending) return;
    setCommittedBrief(brief);
    const name = deriveProjectName(brief.idea);
    createMutation.mutate({ name, brief });
  }

  return (
    <div className="min-h-screen bg-paper">
      <div className="mx-auto px-6 py-12" style={{ maxWidth: 760 }}>
        <nav className="mb-10 font-sans text-[10px] uppercase tracking-[0.2em] text-pencil">
          <span className="opacity-60">studio</span>
          <span className="mx-2 opacity-40">/</span>
          <span className="text-ink">begin a project</span>
        </nav>

        <header className="mb-8">
          <h1 className="font-serif font-medium text-[40px] leading-[0.98] -tracking-[0.02em] mb-3">
            Tell me what you want to <em className="italic text-tomato">make</em>.
          </h1>
          <p className="font-serif text-[16px] text-ink-soft max-w-[560px] leading-[1.55]">
            A sentence or two is plenty. I'll ask 1–3 quick questions, then
            start coding it. Skip any question and I'll guess.
          </p>
        </header>

      {!wizardStarted && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            handleSendIdea();
          }}
          className="mb-8"
        >
          <NotebookPrompt
            label="Idea"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onCmdEnter={handleSendIdea}
            rows={4}
            placeholder="A recipe sharing space for my supper club where guests can sign in, post photos, and vote on who hosts next…"
            hint={<CmdEnterHint />}
            action={
              <Button
                type="submit"
                variant="primary"
                disabled={!draft.trim() || busy}
                data-testid="wizard-send-idea"
              >
                Begin →
              </Button>
            }
            data-testid="wizard-prompt"
          />
        </form>
      )}

      {wizardStarted && (
        <div
          data-testid="wizard-transcript"
          className="bg-paper-2 border border-rule rounded-md min-h-[400px] flex flex-col"
        >
          <ChatMessages
            messages={messages}
            busy={busy}
            onSubmitSurvey={submitSurvey}
            answeredSurveys={answeredSurveys}
            onBeginBrief={handleBegin}
            briefCommitted={committedBrief != null}
            briefBusy={createMutation.isPending}
          />
        </div>
      )}

        {error && (
          <div className="mt-4 px-4 py-3 border border-blood/30 bg-blood/5 font-sans text-[12px] text-blood rounded-md">
            {error.message}
          </div>
        )}
        {createMutation.error && (
          <div
            data-testid="wizard-create-error"
            className="mt-4 px-4 py-3 border border-blood/30 bg-blood/5 font-sans text-[12px] text-blood rounded-md"
          >
            Couldn't create project: {createMutation.error instanceof Error ? createMutation.error.message : String(createMutation.error)}
          </div>
        )}
      </div>
    </div>
  );
}
