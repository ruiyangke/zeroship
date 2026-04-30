// ─── ProjectWorkspace — shell for /p/:appId/* ───────────────────
//
// Layout (atelier):
//   [────────────────────  TopBar  ───────────────────]
//   [     PreviewTab / FilesTab / …     | ChatRail      ]
//   [    "If you're curious:" drawer    |               ]
//
// Preview is the page (1fr). Chat is a 380px desk-side notebook.
// Dev tabs live in a tiny drawer at the bottom of the preview area
// — not in the topbar, not visible by default.

import { useEffect, useMemo, useRef, useState } from "react";
import { Outlet, useNavigate, useOutletContext, useParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getApp, type AppRecord, appPreviewUrl } from "../api";
import { useBuilderChat, type BuilderChat } from "../builder/useBuilderChat";
import { ChatRail } from "./ChatRail";
import { TopBar } from "../components/TopBar";
import { LiveBanner } from "../components/LiveBanner";

export interface WorkspaceCtx {
  appId: string;
  app: AppRecord | undefined;
  appQueryError: Error | null;
  chat: BuilderChat;
  /** Bumped after every successful deploy — tabs use it as a key. */
  deployVersion: number;
  bumpDeployVersion: () => void;
}

export function useWorkspace(): WorkspaceCtx {
  return useOutletContext<WorkspaceCtx>();
}

const PENDING_PROMPT_KEY = "zeroship_pending_prompt";

export function ProjectWorkspace() {
  const { appId } = useParams<{ appId: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [deployVersion, setDeployVersion] = useState(0);
  const [showLiveBanner, setShowLiveBanner] = useState(false);
  const [shippedAt, setShippedAt] = useState<number | null>(null);

  if (!appId) {
    navigate("/", { replace: true });
    return null;
  }

  const { data: app, error: appQueryError } = useQuery({
    queryKey: ["app", appId],
    queryFn: () => getApp(appId),
    refetchOnWindowFocus: false,
    retry: false,
  });

  const context = useMemo(() => ({ app_id: appId, app_name: app?.name }), [appId, app?.name]);

  const lastBumpRef = useRef(0);
  const bumpDeployVersion = () => {
    const now = Date.now();
    if (now - lastBumpRef.current < 200) return;
    lastBumpRef.current = now;
    setDeployVersion((v) => v + 1);
  };

  const chat = useBuilderChat({
    appId,
    context,
    onDeploy: () => {
      queryClient.invalidateQueries({ queryKey: ["app", appId] });
      setTimeout(bumpDeployVersion, 600);
      setShippedAt(Date.now());
      setShowLiveBanner(true);
    },
    onAppCreated: (newAppId) => {
      navigate(`/p/${newAppId}/preview`, { replace: true });
    },
  });

  // Auto-send the pending prompt from /new on first mount of a fresh project
  const sentRef = useRef<string | null>(null);
  useEffect(() => {
    if (sentRef.current === appId) return;
    if (chat.status !== "idle") return;
    if (chat.messages.length > 0) return;
    let pending: string | null = null;
    try {
      pending = sessionStorage.getItem(PENDING_PROMPT_KEY);
      if (pending) sessionStorage.removeItem(PENDING_PROMPT_KEY);
    } catch {}
    if (!pending) return;
    sentRef.current = appId;
    void chat.actions.send(pending);
  }, [appId, chat]);

  const ctx: WorkspaceCtx = {
    appId,
    app,
    appQueryError: appQueryError ?? null,
    chat,
    deployVersion,
    bumpDeployVersion,
  };

  const status = chat.status;
  const statusLabel = describeStatus(chat);

  return (
    <div className="h-screen flex flex-col bg-paper">
      <TopBar
        projectName={app?.name ?? "loading…"}
        crumb={[{ label: "studio", to: "/" }]}
        center={
          status !== "idle" && (
            <span className="inline-flex items-baseline gap-2.5 font-serif text-[13px] text-ink-soft italic">
              <span
                className="inline-block size-[7px] rounded-full bg-tomato pulse-dot self-center"
                aria-hidden="true"
              />
              <strong className="font-serif font-medium text-ink not-italic" data-testid="status-title">{statusLabel.title}</strong>
              {statusLabel.detail && (
                <span className="font-sans text-[10.5px] uppercase tracking-[0.16em] text-pencil not-italic">
                  {statusLabel.detail}
                </span>
              )}
            </span>
          )
        }
        right={
          app && (
            <a
              href={app.deploy_hash ? appPreviewUrl(app.name) : "#"}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-2 px-3 py-1.5 border border-rule rounded-full bg-paper-2 font-mono text-[11px] text-ink-soft hover:border-ink hover:text-ink transition-colors"
              style={{ textDecoration: "none" }}
              data-testid="topbar-url"
            >
              <span className="size-[5px] rounded-full bg-tomato pulse-dot" aria-hidden="true" />
              {app.name}.zeroship.app
            </a>
          )
        }
      />

      {showLiveBanner && app && (
        <LiveBanner
          appName={app.name}
          appUrl={`${app.name}.zeroship.app`}
          shippedAgo={fmtAgo(shippedAt)}
          onDismiss={() => setShowLiveBanner(false)}
          onShare={() => {
            const url = `https://${app.name}.zeroship.app`;
            navigator.clipboard?.writeText(url);
          }}
          onCustomDomain={() => navigate(`/p/${appId}/settings#domain`)}
        />
      )}

      <div className="flex-1 grid min-h-0" style={{ gridTemplateColumns: "1fr 380px" }}>
        <main className="min-h-0 min-w-0 overflow-hidden flex flex-col" data-testid="tab-content">
          <Outlet context={ctx} />
        </main>
        <aside className="border-l border-rule bg-paper-2 min-h-0 min-w-0 overflow-hidden flex flex-col" data-testid="chat-rail">
          <ChatRail chat={chat} appName={app?.name} />
        </aside>
      </div>
    </div>
  );
}

function describeStatus(chat: BuilderChat): { title: string; detail?: string } {
  switch (chat.status) {
    case "thinking":     return { title: "thinking" };
    case "calling-tool": return { title: "running a tool" };
    case "deploying":    return { title: "deploying" };
    case "error":        return { title: "something went wrong" };
    default:             return { title: "" };
  }
}

function fmtAgo(t: number | null): string {
  if (!t) return "just now";
  const sec = Math.floor((Date.now() - t) / 1000);
  if (sec < 60) return "just now";
  return `${Math.floor(sec / 60)} min ago`;
}
