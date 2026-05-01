// Wizard surface — pre-coding clarification chat. Per spec §8.2.7 +
// §4.8.2b: this page talks to the wizard runtime (plain LangGraph,
// `apps/zeroship-builder/src/server/_wizard.ts`), NOT Builder. The
// wizard halts on data-survey chunks; the user answers; once the LLM
// decides the brief is concrete enough it emits a terminal data-brief
// chunk that we render as a BriefCard with a Begin button.
//
// On Begin: per spec §8.2.4, this is where project creation +
// sandbox provisioning happen synchronously and we navigate to
// /p/<id>/preview where the WORKSPACE Builder takes over. Plan 01
// hasn't wired the project-creation API yet (the orphan-tree
// `createApp` import in the old NewProject.tsx is broken) — Begin
// here is a placeholder that surfaces the brief to the user so we
// can prove the wire end-to-end. Plan 03 picks up the real handoff.

import { useMemo, useState } from "react";
import { useChat } from "@ai-sdk/react";
import { rpc, wizardTransport } from "../api";
import { NotebookPrompt, CmdEnterHint } from "../components/NotebookPrompt";
import { Button } from "../components/Button";
import { ChatMessages } from "../workspace/chat/ChatMessages";
import type { Brief, SurveyResponse } from "../types/chat";

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

  const [draft, setDraft] = useState("");
  const [committedBrief, setCommittedBrief] = useState<Brief | null>(null);
  const [answeredSurveys, setAnsweredSurveys] = useState<Set<string>>(() => new Set());

  const { messages, sendMessage, status, error } = useChat({
    id: sessionId,
    transport: wizardTransport(rpc.wizard),
    onError: (err) => console.error("[wizard]", err),
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
    if (committedBrief) return;
    setCommittedBrief(brief);
    // Plan 01 placeholder. Plan 03 wires this to:
    //   1. POST createApp({ name, slug, plan }) — derive name/slug
    //      from brief.summary or prompt the user via inferred fields
    //   2. sessionStorage.setItem("zeroship_pending_brief", JSON.stringify(brief))
    //   3. navigate(`/p/${app.id}/preview`)
    //
    // For now: surface the brief so we can verify the wire visually.
    console.info("[wizard] begin", brief);
    if (typeof window !== "undefined") {
      window.alert(
        `Brief committed (Plan 01 stub).\n\nSummary:\n${brief.summary}\n\nPlan 03 will create the project and hand off to Builder.`,
      );
    }
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
          />
        </div>
      )}

        {error && (
          <div className="mt-4 px-4 py-3 border border-blood/30 bg-blood/5 font-sans text-[12px] text-blood rounded-md">
            {error.message}
          </div>
        )}
      </div>
    </div>
  );
}
