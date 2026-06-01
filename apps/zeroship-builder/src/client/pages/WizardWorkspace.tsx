// Wizard surface — pre-coding clarification chat. Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.7 +
// §4.8.2b: this page talks to the wizard runtime (plain LangGraph,
// `apps/zeroship-builder/src/server/wizard.ts`), NOT Builder. The
// wizard halts on data-survey chunks; the user answers; once the LLM
// decides the brief is concrete enough it emits a terminal data-brief
// chunk that we render as a BriefCard with a Begin button.
//
// On Begin: per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.4, this is where project creation +
// sandbox provisioning happen synchronously and we navigate to
// /p/<id>/preview where the WORKSPACE Builder takes over. We:
//   1. derive a project name from the brief idea (first 3 words),
//   2. createProject({ name }) — a KV-local project (per-thread sandbox
//      session); the console is a pure creator app with no control plane,
//   3. stash the brief in sessionStorage under `zeroship_pending_brief`,
//   4. navigate to /p/<project.id>/preview where WorkspaceShell consumes
//      the stash and seeds ChatRail with a synthesised first message.
//
// Crystal: the inline minimal frame (kept — see the PageFrame note
// below) is rebuilt over @zeroship/ui Container + Stack/Cluster layout
// primitives, Card surfaces, and Button. Bespoke editorial bits (the
// crumbs eyebrow, the display title with its accent "make", the lede,
// the validation/stuck/error bands) live in the co-located
// WizardWorkspace.css reading --zs-* tokens. The already-migrated
// NotebookPrompt / ChatMessages are reused as-is. Public component
// interface, data hooks, routing, and test hooks are unchanged.

import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useChat } from "@ai-sdk/react";
import { useMutation } from "@tanstack/react-query";
import { Button, Card, Cluster, Container, Stack } from "@zeroship/ui";
import { createProject, rpc, wizardTransport } from "../api";
import { NotebookPrompt, CmdEnterHint } from "../components/NotebookPrompt";
import { ChatMessages } from "../workspace/chat/ChatMessages";
import type { Brief, Survey, SurveyResponse } from "../types/chat";
import { lsGet, lsSet } from "../lib/storage";
import { track } from "../lib/analytics";
import "./WizardWorkspace.css";

const PENDING_BRIEF_KEY = "zeroship_pending_brief";
const PENDING_PROMPT_KEY = "zeroship_pending_prompt";
const FIRST_RUN_KEY = "zeroship_first_run";

// Wall budget after which we surface a "stuck?" retry. Generous so
// healthy long survey/structured-output round-trips don't trip it —
// the wizard's normal cadence is well under 30s per turn.
const STUCK_TIMEOUT_MS = 60_000;

/**
 * Derive a project name from a free-text idea. Keep it to a tidy
 * 1-64-char slug (alphanumeric + hyphen): take the first three words,
 * lowercase, slugified to a hyphen-joined string, then suffix with a
 * short random token so two projects from similar ideas don't collide.
 * Falls back to "untitled" when the idea is empty. (The id is what the
 * route + sandbox key off; the name is purely a display label now.)
 */
function deriveProjectName(idea: string): string {
  const words = idea.trim().split(/\s+/).filter(Boolean).slice(0, 3);
  const slugBase = words
    .map((w) => w.toLowerCase().replace(/[^a-z0-9]+/g, ""))
    .filter(Boolean)
    .join("-");
  const base = slugBase.length > 0 ? slugBase : "untitled";
  // 6-char base36 suffix (~36^6 ≈ 2B values) — collision-resistant
  // for any single user without making names ugly. Crypto.randomUUID
  // is available everywhere we render (browser + V8 dev runtime).
  const suffix = (
    crypto.getRandomValues(new Uint32Array(1))[0] % 36 ** 6
  )
    .toString(36)
    .padStart(6, "0");
  return `${base}-${suffix}`.slice(0, 64);
}

// Inline minimal frame instead of PageFrame because the latter pulls
// in TopBar → useAuth → AuthProvider, which lives in the orphan tree
// from the pre-redesign auth pages and has broken imports. A later cleanup
// restores AuthProvider at the App root and we'll switch to PageFrame
// then. Keeping the wizard layout decoupled also matches §4.8.2b: the
// wizard runs *before* a project (and arguably before a real auth
// session in some flows), so a lightweight standalone frame fits.

export function WizardWorkspace() {
  const [searchParams] = useSearchParams();
  // Stable session id for the wizard's checkpointer. One id per
  // mount — refresh = clean wizard. The id is also threaded through
  // the resume protocol (server uses it as LangGraph thread_id).
  const sessionId = useMemo(() => crypto.randomUUID(), []);
  const navigate = useNavigate();

  const [draft, setDraft] = useState(() => {
    const fromUrl = searchParams.get("prompt")?.trim();
    if (fromUrl) {
      try {
        sessionStorage.removeItem(PENDING_PROMPT_KEY);
      } catch {
        // Storage can be disabled; the URL param path still works.
      }
      return fromUrl;
    }
    try {
      const fromStorage = sessionStorage.getItem(PENDING_PROMPT_KEY)?.trim();
      if (fromStorage) {
        sessionStorage.removeItem(PENDING_PROMPT_KEY);
        return fromStorage;
      }
    } catch {
      // Storage can be disabled; the URL param path still works.
    }
    return "";
  });
  const [committedBrief, setCommittedBrief] = useState<Brief | null>(null);
  const [answeredSurveys, setAnsweredSurveys] = useState<Set<string>>(() => new Set());
  const [validationHint, setValidationHint] = useState<string | null>(null);

  // First-run hint card — visible only the very first time a user
  // hits /new on this browser. Once they submit an idea (or the flag
  // is already set), it's hidden forever. Reads localStorage once on
  // mount via lazy initializer so the value is stable across renders.
  const [showFirstRunHint, setShowFirstRunHint] = useState<boolean>(() => {
    return lsGet(FIRST_RUN_KEY) === null;
  });

  // Track when the wizard might be "stuck" — streaming for too long
  // with no fresh content. We approximate freshness by watching status
  // transitions; on every status change we reset the timer.
  const [stuck, setStuck] = useState(false);

  // Latency timer for `project.creation_completed` analytics. Stamped
  // on the first sendMessage; consumed on createApp success.
  const firstSendAt = useRef<number | null>(null);
  const beginClicked = useRef(false);
  const surveysShown = useRef<Set<string>>(new Set());
  const creationStartedAt = useRef<number | null>(null);

  const { messages, sendMessage, status, error, stop, regenerate } = useChat({
    id: sessionId,
    transport: wizardTransport(rpc.wizard),
    onError: (err) => console.error("[wizard]", err),
  });

  // createProject mutation — `useMutation` gives us isPending + error
  // without manual state. On success we stash the brief and navigate;
  // on failure the error band at the bottom of the page renders it.
  const createMutation = useMutation({
    mutationFn: async (vars: { name: string; brief: Brief }) => {
      const app = await createProject({ name: vars.name });
      return { app, brief: vars.brief };
    },
    onSuccess: ({ app, brief }) => {
      try {
        sessionStorage.setItem(PENDING_BRIEF_KEY, JSON.stringify(brief));
      } catch {
        // Quota / disabled storage — workspace still loads, the chat
        // rail just won't auto-seed. User can paste it manually.
      }
      const elapsed = firstSendAt.current
        ? Date.now() - firstSendAt.current
        : null;
      track("project.creation_completed", {
        app_id: app.id,
        latency_ms: elapsed,
      });
      navigate(`/p/${app.id}/preview`);
    },
    onError: (err) => {
      track("project.creation_failed", {
        message: err instanceof Error ? err.message : String(err),
      });
    },
  });

  const busy = status === "submitted" || status === "streaming";
  // Disable composer once the wizard has started — the wizard takes
  // over via surveys. The user goes back to typing only when they
  // refresh or navigate away.
  const wizardStarted = messages.length > 0;

  // Watch for newly-arrived data-survey parts and emit `survey.shown`
  // exactly once per token. We re-derive the set of currently-visible
  // survey tokens on every render and diff against the ref so the
  // event fires even when the parent re-renders for unrelated state.
  useEffect(() => {
    for (const m of messages) {
      if (m.role !== "assistant") continue;
      for (const part of m.parts) {
        if (part.type !== "data-survey") continue;
        const data = (part as { data?: { token?: unknown; survey?: unknown } }).data;
        const token = typeof data?.token === "string" ? data.token : null;
        const survey = data?.survey as Survey | undefined;
        if (!token || surveysShown.current.has(token)) continue;
        if (!survey || !Array.isArray(survey.questions)) continue;
        surveysShown.current.add(token);
        track("survey.shown", {
          token,
          question_count: survey.questions.length,
          kinds: survey.questions.map((q) => q.kind?.type ?? "unknown"),
          source: "wizard",
        });
      }
    }
  }, [messages]);

  // Stuck-detector. Whenever streaming starts, arm a one-shot timer.
  // Status transitions (any change) cancel/reset it — useChat updates
  // status on every chunk arrival, so as long as data is flowing the
  // timer keeps resetting and never fires.
  useEffect(() => {
    if (status !== "streaming" && status !== "submitted") {
      setStuck(false);
      return;
    }
    setStuck(false);
    const id = window.setTimeout(() => setStuck(true), STUCK_TIMEOUT_MS);
    return () => window.clearTimeout(id);
  }, [status, messages.length]);

  // Abandonment analytics: if the user mounts the wizard, never clicks
  // Begin (creationStartedAt unset OR no commit), and unmounts — log
  // it. Useful signal for "people bounce off the wizard" diagnostics.
  useEffect(() => {
    return () => {
      if (creationStartedAt.current && !beginClicked.current) {
        track("project.creation_abandoned", {
          turns: 0, // we don't have the messages array in cleanup; placeholder
        });
      }
    };
  }, []);

  function handleSendIdea() {
    const text = draft.trim();
    if (!text) {
      setValidationHint("Describe your idea — even one sentence is enough.");
      return;
    }
    if (busy || wizardStarted) return;
    setValidationHint(null);
    if (showFirstRunHint) {
      lsSet(FIRST_RUN_KEY, "completed");
      setShowFirstRunHint(false);
    }
    if (firstSendAt.current === null) {
      firstSendAt.current = Date.now();
      creationStartedAt.current = Date.now();
      track("project.creation_started", {
        flow: "default",
        prompt_length: text.length,
        has_image: false,
      });
    }
    sendMessage({ text });
  }

  function submitSurvey(token: string, response: SurveyResponse) {
    if (busy || answeredSurveys.has(token)) return;
    setAnsweredSurveys((s) => {
      const next = new Set(s);
      next.add(token);
      return next;
    });
    if (response.skipped) {
      track("survey.skipped", { token, source: "wizard" });
    } else {
      const answered = Object.keys(response.answers).length;
      track("survey.answered", {
        token,
        answered_count: answered,
        skipped: false,
        source: "wizard",
      });
    }
    const value = response.skipped
      ? { skipped: true }
      : { skipped: false, answers: response.answers };
    sendMessage(undefined, { body: { resume: { token, value } } });
  }

  function handleBegin(brief: Brief) {
    if (committedBrief || createMutation.isPending) return;
    beginClicked.current = true;
    setCommittedBrief(brief);
    const name = deriveProjectName(brief.idea);
    createMutation.mutate({ name, brief });
  }

  // Stuck-recovery: try regenerate (re-run the last turn against the
  // server). If the SDK doesn't expose regenerate (older version), fall
  // back to stop + a fresh sendMessage with the same idea text.
  function handleRetryStuck() {
    setStuck(false);
    track("wizard.retry_stuck");
    try {
      // regenerate is the canonical re-run; documented on AbstractChat.
      regenerate();
    } catch {
      // Best-effort fallback — abort and let the user resend manually.
      stop();
    }
  }

  return (
    <div className="zb-wizard">
      <Container size="md" className="zb-wizard__column">
        <Stack gap={8}>
          <Stack gap={6}>
            <Cluster gap={2} className="zb-wizard__crumbs" aria-label="Breadcrumb">
              <span className="zb-wizard__crumb-muted">studio</span>
              <span className="zb-wizard__crumb-sep" aria-hidden="true">
                /
              </span>
              <span className="zb-wizard__crumb-current">begin a project</span>
            </Cluster>

            <Stack gap={3} asChild>
              <header>
                <h1 className="zb-wizard__title">
                  Tell me what you want to{" "}
                  <em className="zb-wizard__title-em">make</em>.
                </h1>
                <p className="zb-wizard__lede">
                  A sentence or two is plenty. I'll ask 1–3 quick questions, then
                  start coding it. Skip any question and I'll guess.
                </p>
              </header>
            </Stack>
          </Stack>

          {showFirstRunHint && !wizardStarted && (
            <Card
              variant="outline"
              data-testid="wizard-first-run-hint"
              className="zb-wizard__first-run"
            >
              <Stack gap={2}>
                <span className="zb-wizard__first-run-eyebrow">
                  first project? here's what to expect
                </span>
                <p className="zb-wizard__first-run-body">
                  Type your idea below in plain English. I'll ask a couple of
                  quick questions to make sure we're on the same page, then I'll
                  start coding. Most projects ship in under a minute.
                </p>
              </Stack>
            </Card>
          )}

          {!wizardStarted && (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                handleSendIdea();
              }}
            >
              <Stack gap={2}>
                <NotebookPrompt
                  label="Idea"
                  value={draft}
                  onChange={(e) => {
                    setDraft(e.target.value);
                    if (validationHint) setValidationHint(null);
                  }}
                  onCmdEnter={handleSendIdea}
                  rows={4}
                  placeholder="A recipe sharing space for my supper club where guests can sign in, post photos, and vote on who hosts next…"
                  hint={<CmdEnterHint />}
                  action={
                    <Button
                      type="submit"
                      variant="filled"
                      disabled={!draft.trim() || busy}
                      data-testid="wizard-send-idea"
                    >
                      Begin →
                    </Button>
                  }
                  data-testid="wizard-prompt"
                />
                {validationHint && (
                  <div
                    data-testid="wizard-validation-hint"
                    className="zb-wizard__validation"
                  >
                    {validationHint}
                  </div>
                )}
              </Stack>
            </form>
          )}

          {wizardStarted && (
            <Card
              variant="outline"
              data-testid="wizard-transcript"
              className="zb-wizard__transcript"
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
            </Card>
          )}

          {stuck && busy && (
            <Cluster
              justify="between"
              gap={3}
              data-testid="wizard-stuck"
              className="zb-wizard__band"
            >
              <span className="zb-wizard__band-text">
                Builder seems stuck. Want to try again?
              </span>
              <Button
                type="button"
                variant="gray"
                size="small"
                onClick={handleRetryStuck}
                data-testid="wizard-retry"
              >
                Retry
              </Button>
            </Cluster>
          )}

          {error && (
            <div className="zb-wizard__error">{error.message}</div>
          )}
          {createMutation.error && (
            <div data-testid="wizard-create-error" className="zb-wizard__error">
              Couldn't create the project — please try again. (
              {createMutation.error instanceof Error
                ? createMutation.error.message
                : String(createMutation.error)}
              )
            </div>
          )}
        </Stack>
      </Container>
    </div>
  );
}
