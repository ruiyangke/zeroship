import { useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { getApp } from "../api";
import { TopBar } from "./TopBar";
import { CanvasPills, type CanvasPillId } from "./CanvasPills";
import { PreviewCanvasStub } from "./PreviewCanvasStub";
import { ChatRail } from "./chat/ChatRail";
import { briefSchema, type Brief } from "../types/chat";

const PENDING_BRIEF_KEY = "zeroship_pending_brief";

export interface WorkspaceShellProps {
  /** When set, overrides the URL param. The router doesn't need this
   *  but tests / embeds may want to mount the shell directly with a
   *  fixed appId. */
  appId?: string;
  /** Override for tests / catch-all route. When the URL has no
   *  `:appId` and no override is supplied, the shell renders an
   *  "untitled" placeholder so the catch-all stays useful. */
  projectName?: string;
}

/**
 * Read the pending brief stash dropped by the wizard's `handleBegin`
 * (sessionStorage key `zeroship_pending_brief`). One-shot consume:
 * the entry is removed on read so a refresh of /p/:id/preview doesn't
 * re-seed the chat. Returns null when nothing is stashed or the JSON
 * is malformed.
 */
function consumePendingBrief(): Brief | null {
  try {
    const raw = sessionStorage.getItem(PENDING_BRIEF_KEY);
    if (!raw) return null;
    sessionStorage.removeItem(PENDING_BRIEF_KEY);
    const parsed = briefSchema.safeParse(JSON.parse(raw));
    return parsed.success ? parsed.data : null;
  } catch {
    return null;
  }
}

export function WorkspaceShell({ appId: appIdProp, projectName: projectNameProp }: WorkspaceShellProps) {
  const params = useParams<{ appId: string }>();
  const appId = appIdProp ?? params.appId;
  const [active, setActive] = useState<CanvasPillId>("preview");

  // One-shot brief consume on mount. We keep the value in state so
  // re-renders during the chat's first turn don't re-trigger the seed
  // effect inside ChatRail (which gates on messages.length === 0
  // anyway, but defence-in-depth).
  const seedBrief = useMemo(() => (appId ? consumePendingBrief() : null), [appId]);

  const appQuery = useQuery({
    queryKey: ["app", appId],
    queryFn: () => getApp(appId!),
    enabled: !!appId,
  });

  // Loading / error gates only fire when we have an appId. The
  // catch-all route (no appId) bypasses them and renders the legacy
  // "untitled" shell so existing tests keep passing.
  if (appId && appQuery.isLoading) {
    return (
      <div className="h-screen flex items-center justify-center bg-paper">
        <div data-testid="workspace-loading" className="font-serif italic text-ink-soft">
          loading…
        </div>
      </div>
    );
  }

  if (appId && appQuery.error) {
    return (
      <div className="h-screen flex items-center justify-center bg-paper">
        <div data-testid="workspace-error" className="text-center">
          <h1 className="font-serif italic text-2xl text-ink mb-2">Project not found</h1>
          <p className="font-serif text-ink-soft mb-4">
            We couldn't load that project. It may have been deleted or you don't have access.
          </p>
          <Link
            to="/"
            data-testid="workspace-error-home"
            className="font-serif italic text-tomato hover:opacity-80"
          >
            ← Back to home
          </Link>
        </div>
      </div>
    );
  }

  const projectName = appQuery.data?.name ?? projectNameProp ?? "untitled";

  return (
    <div className="h-screen flex flex-col bg-paper">
      <TopBar
        projectName={projectName}
        center={
          <div className="flex items-center gap-3">
            <CanvasPills active={active} onChange={setActive} />
          </div>
        }
        right={
          <a
            href="#"
            data-testid="topbar-url"
            className="inline-flex items-center gap-2 px-3 py-1.5 border border-rule rounded-full bg-paper-2 font-mono text-[11px] text-ink-soft hover:border-ink hover:text-ink"
            style={{ textDecoration: "none" }}
          >
            <span className="size-[5px] rounded-full bg-ivy pulse-dot" aria-hidden="true" />
            {projectName}.zeroship.app
          </a>
        }
        accountInitials="ZS"
      />

      <div
        className="flex-1 grid min-h-0"
        style={{ gridTemplateColumns: "1fr 320px" }}
      >
        <main data-testid="canvas-area" className="min-h-0 min-w-0 overflow-hidden flex flex-col">
          {/* Plan 01 only renders preview; pill switching is wired but other
              canvases are placeholders. Plans 03–06 fill them in. */}
          {active === "preview" && <PreviewCanvasStub />}
          {active !== "preview" && (
            <div className="h-full flex items-center justify-center text-ink-soft font-serif italic">
              "{active}" canvas — coming in a later plan.
            </div>
          )}
        </main>
        <aside className="border-l border-rule min-h-0 min-w-0 overflow-hidden flex flex-col">
          <ChatRail appName={projectName} seedBrief={seedBrief ?? undefined} />
        </aside>
      </div>
    </div>
  );
}

