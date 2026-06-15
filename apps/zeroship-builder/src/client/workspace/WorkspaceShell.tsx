// ─── WorkspaceShell — crystal workspace frame ─────────────────────
//
// The main app shell for /p/:appId/*. Rebuilt over the @zeroship/ui
// AppShell: a Header (the workspace TopBar with canvas pills + tier
// toggle) and a Body whose Main is the canvas area and whose end-side
// Sidebar rail is the chat. On phones the rail drops out of the layout
// and the chat re-mounts as a full-screen DS Drawer.
//
// Crystal: AppShell / Split (via AppShell.Body) own the frame; Center
// arranges the loading / error / not-found states; Button + Drawer
// replace the hand-rolled controls; the few bespoke bits (the chat
// toggle / tour glyph chips, the live-URL pill, the phone-drawer chat
// surface) live in the co-located WorkspaceShell.css against --zs-*
// tokens. The public component interface, URL-suffix canvas routing,
// the maker/ops/code tier system + persistence, the per-canvas
// ErrorBoundary wrappers, every data hook, and ALL data-testid hooks
// are unchanged — only presentation moved to crystal.

import { Suspense, lazy, useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { AppShell, Button, Center, Cluster, Drawer } from "@zeroship/ui";
import { getProject } from "../api";
import { TopBar } from "./TopBar";
import { CanvasPills, pillsForTier, type CanvasPillId, type CanvasTier } from "./CanvasPills";
import { briefSchema, type Brief } from "../types/chat";
import { ProductTour } from "../components/ProductTour";
import { ErrorBoundary } from "../components/ErrorBoundary";
import { lsGet, lsSet } from "../lib/storage";
import { useMediaQuery } from "../lib/useMediaQuery";
import "./WorkspaceShell.css";

const PENDING_BRIEF_KEY = "zeroship_pending_brief";
const TIER_KEY = "zeroship_canvas_tier";

const PreviewCanvas = lazy(() =>
  import("./PreviewCanvas").then((m) => ({ default: m.PreviewCanvas })),
);
const FilesCanvas = lazy(() =>
  import("./canvases/FilesCanvas").then((m) => ({ default: m.FilesCanvas })),
);
const LogsCanvas = lazy(() =>
  import("./canvases/LogsCanvas").then((m) => ({ default: m.LogsCanvas })),
);
const EnvCanvas = lazy(() =>
  import("./canvases/EnvCanvas").then((m) => ({ default: m.EnvCanvas })),
);
const SettingsCanvas = lazy(() =>
  import("./canvases/SettingsCanvas").then((m) => ({ default: m.SettingsCanvas })),
);
const ChatRail = lazy(() =>
  import("./chat/ChatRail").then((m) => ({ default: m.ChatRail })),
);

function readTier(): CanvasTier {
  const v = lsGet(TIER_KEY);
  if (v === "maker" || v === "ops" || v === "code") return v;
  return "maker";
}

export interface WorkspaceShellProps {
  /** When set, overrides the URL param. The router doesn't need this
   *  but tests / embeds may want to mount the shell directly with a
   *  fixed appId. */
  appId?: string;
  /** Override for tests / embeds. When the URL has no
   *  `:appId` and no override is supplied, the shell renders an
   *  "untitled" placeholder. */
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
  const params = useParams<{ appId: string; "*": string }>();
  const navigate = useNavigate();
  const appId = appIdProp ?? params.appId;
  const routeRest = params["*"] ?? "";
  const routeActive = canvasFromRouteRest(routeRest);
  const hasInvalidCanvasPath = routeRest !== "" && routeActive === null;
  const [active, setActiveState] = useState<CanvasPillId>(() => routeActive ?? "preview");
  const [tier, setTierState] = useState<CanvasTier>(() => readTier());
  const [tourOpen, setTourOpen] = useState(false);

  // Tier change side-effects: persist + ensure the active pill stays
  // visible. If the new tier hides the current pill, snap to "preview"
  // (always available) — silently switching is friendlier than hiding
  // the pill while keeping the canvas visible.
  function setTier(next: CanvasTier) {
    setTierState(next);
    lsSet(TIER_KEY, next);
    const visible = pillsForTier(next);
    if (!visible.includes(active)) {
      setActive("preview");
    }
  }
  // Phone breakpoint: < 768px. The chat rail collapses out of the
  // layout and becomes a togglable full-screen drawer. Tablet and up
  // keep the 320px sidebar.
  const isPhone = useMediaQuery("(max-width: 767px)");
  const [chatOpen, setChatOpen] = useState(false);

  // One-shot brief consume on mount. We keep the value in state so
  // re-renders during the chat's first turn don't re-trigger the seed
  // effect inside ChatRail (which gates on messages.length === 0
  // anyway, but defence-in-depth).
  const seedBrief = useMemo(() => (appId ? consumePendingBrief() : null), [appId]);

  useEffect(() => {
    setActiveState(routeActive ?? "preview");
    const requiredTier = routeActive ? tierForCanvas(routeActive) : null;
    if (requiredTier === "code" && tier !== "code") {
      setTierState("code");
      lsSet(TIER_KEY, "code");
    } else if (requiredTier === "ops" && tier === "maker") {
      setTierState("ops");
      lsSet(TIER_KEY, "ops");
    }
  }, [routeActive, routeRest, tier]);

  const appQuery = useQuery({
    queryKey: ["app", appId],
    queryFn: () => getProject(appId!),
    enabled: !!appId,
  });

  // Loading / error gates only fire when we have an appId. Test embeds
  // without an appId bypass them and render the local shell.
  if (appId && appQuery.isLoading) {
    return (
      <Center minHeight="100dvh" className="zb-ws__gate">
        <span data-testid="workspace-loading" className="zb-ws__gate-loading">
          loading…
        </span>
      </Center>
    );
  }

  if (appId && appQuery.error) {
    return (
      <Center minHeight="100dvh" className="zb-ws__gate">
        <div data-testid="workspace-error" className="zb-ws__gate-body">
          <h1 className="zb-ws__gate-title">Project not found</h1>
          <p className="zb-ws__gate-lede">
            We couldn't load that project. It may have been deleted or you don't have access.
          </p>
          <Link to="/home" data-testid="workspace-error-home" className="zb-ws__gate-link">
            ← Back to home
          </Link>
        </div>
      </Center>
    );
  }

  const projectName = appQuery.data?.name ?? projectNameProp ?? "untitled";

  if (hasInvalidCanvasPath) {
    return (
      <Center minHeight="100dvh" className="zb-ws__gate">
        <div className="zb-ws__gate-body">
          <h1 className="zb-ws__gate-title">Canvas not found</h1>
          <p className="zb-ws__gate-lede">That workspace view does not exist.</p>
          <Button variant="plain" onClick={() => setActive("preview")}>
            Back to preview
          </Button>
        </div>
      </Center>
    );
  }

  function setActive(next: CanvasPillId) {
    setActiveState(next);
    if (!appId) return;
    navigate(`/p/${encodeURIComponent(appId)}/${next}`, { replace: false });
  }

  // Each canvas is wrapped in its own ErrorBoundary so a render crash in
  // one pane does not blank the whole workspace. The fallback at the
  // bottom keeps each pill clickable when an appId is not available.
  const canvasContent = (
    <Suspense
      fallback={
        <Center inline minHeight="100%" className="zb-ws__no-project">
          loading…
        </Center>
      }
    >
      {active === "preview" && (
        <ErrorBoundary label="the preview"><PreviewCanvas appId={appId} /></ErrorBoundary>
      )}
      {active === "files" && appId && (
        <ErrorBoundary label="the files canvas"><FilesCanvas appId={appId} /></ErrorBoundary>
      )}
      {active === "logs" && appId && (
        <ErrorBoundary label="the logs canvas"><LogsCanvas appId={appId} /></ErrorBoundary>
      )}
      {active === "env" && appId && (
        <ErrorBoundary label="the env canvas"><EnvCanvas appId={appId} /></ErrorBoundary>
      )}
      {active === "settings" && appId && (
        <ErrorBoundary label="settings">
          <SettingsCanvas appId={appId} app={appQuery.data} />
        </ErrorBoundary>
      )}
      {!appId && active !== "preview" && (
        <Center inline minHeight="100%" className="zb-ws__no-project">
          No project selected.
        </Center>
      )}
    </Suspense>
  );

  return (
    // The chat rail lives on the end edge. On phones it drops out of the
    // layout (`sidebarOpen={false}` collapses the rail to zero width) and
    // we render the rail content only on desktop so the chat's one-shot
    // seed effect doesn't fire in a hidden rail — phone gets the Drawer.
    <AppShell
      sidebarSide="end"
      sidebarWidth="20rem"
      sidebarOpen={!isPhone}
      className="zb-ws"
    >
      {/* AppShell.Header is a <header> banner; TopBar renders its own
          <header>, so drop this wrapper's banner role to keep a single
          banner landmark. The wrapper is neutralised in CSS — TopBar owns
          the bar's chrome. */}
      <AppShell.Header role="none" className="zb-ws__header">
        <TopBar
          projectName={projectName}
          center={
            <Cluster gap={3} align="center">
              <CanvasPills active={active} onChange={setActive} tier={tier} />
              <TierToggle tier={tier} onChange={setTier} />
            </Cluster>
          }
          right={
            <Cluster gap={2} align="center">
              {isPhone && (
                <button
                  type="button"
                  onClick={() => setChatOpen((v) => !v)}
                  data-testid="topbar-chat-toggle"
                  aria-label={chatOpen ? "Close chat" : "Open chat"}
                  aria-expanded={chatOpen}
                  title={chatOpen ? "Close chat" : "Open chat"}
                  className="zb-ws__chip"
                >
                  {/* Speech-bubble glyph; reads as chat at any size. */}
                  <span aria-hidden="true">≡</span>
                </button>
              )}
              <button
                type="button"
                onClick={() => setTourOpen(true)}
                data-testid="topbar-tour"
                aria-label="Take the tour"
                title="Take the tour"
                className="zb-ws__chip"
              >
                ?
              </button>
              <a
                href="#"
                data-testid="topbar-url"
                aria-label={`Live URL: ${projectName}.zeroship.app`}
                className="zb-ws__url"
              >
                <span className="zb-ws__url-dot" aria-hidden="true" />
                {projectName}.zeroship.app
              </a>
            </Cluster>
          }
          accountInitials="ZS"
        />
      </AppShell.Header>

      <AppShell.Body>
        <AppShell.Main data-testid="canvas-area" className="zb-ws__canvas">
          {canvasContent}
        </AppShell.Main>
        {!isPhone && (
          <AppShell.Sidebar className="zb-ws__rail">
            <ErrorBoundary label="the chat rail">
              <Suspense fallback={null}>
                <ChatRail
                  appName={projectName}
                  appId={appId}
                  seedBrief={seedBrief ?? undefined}
                />
              </Suspense>
            </ErrorBoundary>
          </AppShell.Sidebar>
        )}
      </AppShell.Body>

      {/* Phone drawer: full-screen chat overlay. Mounted only while open
          so the rail's onMount seedBrief effect fires correctly, and a
          backdrop-click closes it. */}
      {isPhone && (
        <Drawer open={chatOpen} onOpenChange={setChatOpen}>
          <Drawer.Portal>
            <Drawer.Backdrop />
            <Drawer.Content
              side="end"
              size="full"
              data-testid="chat-drawer"
              aria-label="Chat"
              aria-modal="true"
              className="zb-ws__drawer"
            >
              <Drawer.Header showClose={false} className="zb-ws__drawer-head">
                <Drawer.Title className="zb-ws__drawer-title">Notes &amp; thoughts</Drawer.Title>
                {/* Explicit close so the `chat-drawer-close` testid +
                    aria-label survive (the auto-X carries neither). */}
                <Drawer.Close
                  variant="plain"
                  size="small"
                  data-testid="chat-drawer-close"
                  aria-label="Close chat"
                  className="zb-ws__drawer-close"
                >
                  close
                </Drawer.Close>
              </Drawer.Header>
              <Drawer.Body className="zb-ws__drawer-body">
                <ErrorBoundary label="the chat rail">
                  <Suspense fallback={null}>
                    <ChatRail
                      appName={projectName}
                      appId={appId}
                      seedBrief={seedBrief ?? undefined}
                    />
                  </Suspense>
                </ErrorBoundary>
              </Drawer.Body>
            </Drawer.Content>
          </Drawer.Portal>
        </Drawer>
      )}

      <ProductTour open={tourOpen} onClose={() => setTourOpen(false)} />
    </AppShell>
  );
}

function canvasFromRouteRest(rest: string): CanvasPillId | null {
  if (!rest || rest.includes("/")) return null;
  if (
    rest === "preview" ||
    rest === "files" ||
    rest === "logs" ||
    rest === "env" ||
    rest === "settings"
  ) {
    return rest;
  }
  return null;
}

function tierForCanvas(id: CanvasPillId): CanvasTier {
  if (id === "files") return "code";
  if (id === "logs" || id === "env" || id === "settings") return "ops";
  return "maker";
}

/**
 * Tier toggle — tiny editorial chip that flips the visible pill set
 * between Maker / Ops / Code.
 *
 * Click cycles forward (maker → ops → code → maker). The label shows
 * the next tier.
 */
function TierToggle({
  tier,
  onChange,
}: {
  tier: CanvasTier;
  onChange: (next: CanvasTier) => void;
}) {
  const next: CanvasTier =
    tier === "maker" ? "ops" : tier === "ops" ? "code" : "maker";
  const label =
    next === "ops" ? "+ ops" : next === "code" ? "+ code" : "− maker";
  return (
    <button
      type="button"
      onClick={() => onChange(next)}
      data-testid="tier-toggle"
      data-tier={tier}
      aria-label={`Tier: ${tier}. Click to switch to ${next}.`}
      title={`Tier: ${tier}. Click to switch to ${next}.`}
      className="zb-ws__tier-toggle"
    >
      {label}
    </button>
  );
}
