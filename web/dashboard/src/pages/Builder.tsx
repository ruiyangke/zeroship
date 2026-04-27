// ─── Builder ─────────────────────────────────────────────────────
// Top-level page for the chat-primary AI builder. Two routes:
//
//   /builder             — bootstrap (no app yet); first user message
//                          has the agent provision a new app, then
//                          we navigate to /builder/:newAppId.
//   /builder/:appId      — work on an existing app: chat owns history,
//                          preview shows the deployed URL, optional
//                          code panel exposes server.js for power users.
//
// Layout:
//
//   ┌──── chat ────┬──── code (toggle) ────┬──── preview ────┐
//   │              │ codepanel             │ iframe          │
//   │              │ (collapsed by default)│                 │
//   └──────────────┴───────────────────────┴─────────────────┘

import { useMemo, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { getApp, deployApp } from "../api";
import { Chat } from "../builder/components/Chat";
import { Preview, type PreviewHandle } from "../builder/components/Preview";
import { CodePanel } from "../builder/components/CodePanel";
import { TopBar } from "../builder/components/TopBar";
import { useBuilderChat } from "../builder/useBuilderChat";
import { clearConversation, loadConversation, saveConversation } from "../builder/storage";

export default function Builder() {
  const { appId } = useParams<{ appId?: string }>();

  if (!appId) return <BootstrapBuilder />;
  return <ProjectBuilder key={appId} appId={appId} />;
}

// ─── Bootstrap (no project yet) ──────────────────────────────────
//
// Synthetic id keeps the chat running before the agent calls
// create_app. When create_app succeeds, we copy the chat history to
// the new appId's storage key and navigate over.

function BootstrapBuilder() {
  const navigate = useNavigate();
  const previewRef = useRef<PreviewHandle>(null);
  const [bootstrapId] = useState(() => "draft-" + Math.random().toString(36).slice(2, 10));

  const chat = useBuilderChat({
    appId: bootstrapId,
    onAppCreated: (newAppId) => {
      // Migrate the bootstrap chat history to the real app's storage key
      // so the conversation continues seamlessly after navigation.
      const history = loadConversation(bootstrapId);
      saveConversation(newAppId, history);
      clearConversation(bootstrapId);
      navigate(`/builder/${newAppId}`);
    },
  });

  return (
    <div className="h-screen flex flex-col">
      <TopBar
        appName={null}
        status={chat.status}
        showCode={false}
        onToggleCode={() => {}}
        onReset={chat.actions.reset}
      />
      <div className="flex-1 grid grid-cols-[400px_1fr] min-h-0">
        <div className="border-r border-border min-h-0">
          <Chat chat={chat} />
        </div>
        <Preview ref={previewRef} appName={null} deployVersion={0} />
      </div>
    </div>
  );
}

// ─── Project builder ─────────────────────────────────────────────

function ProjectBuilder({ appId }: { appId: string }) {
  const queryClient = useQueryClient();
  const previewRef = useRef<PreviewHandle>(null);
  const [showCode, setShowCode] = useState(false);
  const [deployVersion, setDeployVersion] = useState(0);

  const { data: app, error: appError } = useQuery({
    queryKey: ["app", appId],
    queryFn: () => getApp(appId),
    refetchOnWindowFocus: false,
  });

  const deployMutation = useMutation({
    mutationFn: (code: string) => deployApp(appId, code),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["app", appId] });
      setDeployVersion((v) => v + 1);
      setTimeout(() => previewRef.current?.reload(), 300);
    },
  });

  const context = useMemo(
    () => ({ app_id: appId, app_name: app?.name }),
    [appId, app?.name],
  );

  const chat = useBuilderChat({
    appId,
    context,
    onDeploy: () => {
      queryClient.invalidateQueries({ queryKey: ["app", appId] });
      setDeployVersion((v) => v + 1);
      // Worker pull lag → small delay before iframe reload.
      setTimeout(() => previewRef.current?.reload(), 600);
    },
  });

  const code = app?.server_js ?? "";
  const appName = app?.name ?? null;

  // Layout columns: 400px chat | (optional) code | preview
  const cols = useMemo(
    () =>
      showCode
        ? "grid-cols-[400px_minmax(360px,1fr)_minmax(0,1.2fr)]"
        : "grid-cols-[400px_1fr]",
    [showCode],
  );

  return (
    <div className="h-screen flex flex-col">
      <TopBar
        appName={appName}
        status={chat.status}
        showCode={showCode}
        onToggleCode={() => setShowCode((v) => !v)}
        onReset={chat.actions.reset}
      />
      <div className={`flex-1 grid ${cols} min-h-0`}>
        <div className="border-r border-border min-h-0">
          <Chat chat={chat} />
        </div>
        {showCode && (
          <div className="min-h-0 min-w-0">
            <CodePanel
              code={code}
              saving={deployMutation.isPending}
              error={deployMutation.error?.message ?? appError?.message ?? null}
              onSave={(next) => deployMutation.mutate(next)}
            />
          </div>
        )}
        <Preview ref={previewRef} appName={appName} deployVersion={deployVersion} />
      </div>
    </div>
  );
}
