// ─── ProjectWorkspace ───────────────────────────────────────────
// Shell for /p/:appId/* routes. Owns:
//   - the chat hook (so state survives tab switches)
//   - the live deploy version counter (preview iframe reload key)
//   - workspace context exposed to children via Outlet context
//
// Layout:
//   ┌───────────────────────── TopBar ──────────────────────────┐
//   ├─ TabNav ──────────────────────────────────────────────────┤
//   ├──────────┬────────────────────────────────────────────────┤
//   │          │                                                │
//   │  Chat    │  <Outlet/> — current tab                       │
//   │  Rail    │                                                │
//   │ (left)   │                                                │
//   └──────────┴────────────────────────────────────────────────┘
//
// Chat is collapsible — power users hide it on the Files tab to
// give CodeMirror more room.

import { useEffect, useMemo, useRef, useState } from "react";
import {
  Outlet,
  useNavigate,
  useOutletContext,
  useParams,
} from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getApp, type AppRecord } from "../api";
import { useBuilderChat, type BuilderChat } from "../builder/useBuilderChat";
import { Chat } from "../builder/components/Chat";
import { consumePendingPrompt } from "../pages/Home";
import { TopBar } from "./components/TopBar";
import { TabNav } from "./components/TabNav";

export interface WorkspaceCtx {
  appId: string;
  app: AppRecord | undefined;
  appQueryError: Error | null;
  chat: BuilderChat;
  /** Bumped after every successful deploy — tabs use it as a
   *  React key on iframes etc. to force fresh content. */
  deployVersion: number;
  /** Tabs can request the iframe / panel reload after their own
   *  mutations (e.g., manual file save). */
  bumpDeployVersion: () => void;
}

/** Read the workspace context from inside any tab via
 *  `const ctx = useWorkspace()`. */
export function useWorkspace(): WorkspaceCtx {
  return useOutletContext<WorkspaceCtx>();
}

interface Props {
  onLogout?: () => void;
}

export function ProjectWorkspace({ onLogout }: Props) {
  const { appId } = useParams<{ appId: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [chatOpen, setChatOpen] = useState(true);
  const [deployVersion, setDeployVersion] = useState(0);

  if (!appId) {
    // /p with no id — bounce to home.
    navigate("/", { replace: true });
    return null;
  }

  const { data: app, error: appQueryError } = useQuery({
    queryKey: ["app", appId],
    queryFn: () => getApp(appId),
    refetchOnWindowFocus: false,
    retry: false,
  });

  const context = useMemo(
    () => ({ app_id: appId, app_name: app?.name }),
    [appId, app?.name],
  );

  const lastBumpRef = useRef(0);
  const bumpDeployVersion = () => {
    // Coalesce bursts (some flows fire reload from multiple places).
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
      // Workers need a beat to pull the new bundle / asset diff.
      setTimeout(bumpDeployVersion, 600);
    },
    onAppCreated: (newAppId) => {
      // Agent provisioned a NEW app (ensureApp inside build_and_publish
      // can do this when the bootstrap id was a placeholder). Migrate
      // chat history and navigate over.
      navigate(`/p/${newAppId}/chat`, { replace: true });
    },
  });

  // If the user landed here via Home's prompt-and-create flow, the
  // pending message is in sessionStorage. Auto-send it once the chat
  // hook has mounted (and only once per appId).
  const sentPromptForRef = useRef<string | null>(null);
  useEffect(() => {
    if (sentPromptForRef.current === appId) return;
    if (chat.status !== "idle") return;
    if (chat.messages.length > 0) return;  // resumed an existing conversation
    const pending = consumePendingPrompt();
    if (!pending) return;
    sentPromptForRef.current = appId;
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

  return (
    <div className="h-screen flex flex-col">
      <TopBar
        projectName={app?.name ?? "loading…"}
        status={chat.status}
        chatOpen={chatOpen}
        onToggleChat={() => setChatOpen((v) => !v)}
        onLogout={onLogout}
      />
      <TabNav appId={appId} />
      <div
        className="flex-1 grid min-h-0"
        style={{
          // The chat rail is first-class — same fixed width Bolt uses.
          // When closed it collapses to 0 so the active tab gets the
          // whole viewport.
          gridTemplateColumns: chatOpen ? "400px 1fr" : "0px 1fr",
        }}
      >
        <aside
          className={
            chatOpen
              ? "border-r border-border min-h-0 min-w-0 overflow-hidden"
              : "min-h-0 min-w-0 overflow-hidden"
          }
          data-testid="chat-rail"
        >
          {chatOpen && <Chat chat={chat} />}
        </aside>
        <main className="min-h-0 min-w-0 overflow-hidden" data-testid="tab-content">
          <Outlet context={ctx} />
        </main>
      </div>
    </div>
  );
}
