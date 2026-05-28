import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { getApp } from "../api";
import { TopBar } from "./TopBar";
import { CanvasPills, pillsForTier, type CanvasPillId, type CanvasTier } from "./CanvasPills";
import { PreviewCanvas } from "./PreviewCanvas";
import { FilesCanvas } from "./canvases/FilesCanvas";
import { LogsCanvas } from "./canvases/LogsCanvas";
import { EnvCanvas } from "./canvases/EnvCanvas";
import { SettingsCanvas } from "./canvases/SettingsCanvas";
import { ChatRail } from "./chat/ChatRail";
import { briefSchema, type Brief } from "../types/chat";
import { LiveBanner } from "../components/LiveBanner";
import { ProductTour } from "../components/ProductTour";
import { ErrorBoundary } from "../components/ErrorBoundary";
import { lsGet, lsSet } from "../lib/storage";
import { track } from "../lib/analytics";
import { useMediaQuery } from "../lib/useMediaQuery";

const PENDING_BRIEF_KEY = "zeroship_pending_brief";
const FIRST_DEPLOY_PREFIX = "zeroship_first_deploy_celebrated_";
const TIER_KEY = "zeroship_canvas_tier";

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
  const [showLiveBanner, setShowLiveBanner] = useState(false);

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
  // grid and becomes a togglable full-screen drawer. Tablet and up
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
    queryFn: () => getApp(appId!),
    enabled: !!appId,
  });

  // First-deploy celebration (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.4). Fires when:
  //   - we have an app id,
  //   - the app's deploy_hash transitions from missing to present,
  //   - and we haven't celebrated this app before on this browser.
  // We persist the per-app flag so a refresh after celebrating doesn't
  // re-fire the banner. The user can also dismiss it explicitly.
  useEffect(() => {
    if (!appId || !appQuery.data) return;
    if (!appQuery.data.deploy_hash) return;
    const flagKey = `${FIRST_DEPLOY_PREFIX}${appId}`;
    if (lsGet(flagKey) === "true") return;
    lsSet(flagKey, "true");
    setShowLiveBanner(true);
    track("project.first_deploy", { app_id: appId });
  }, [appId, appQuery.data]);

  // Loading / error gates only fire when we have an appId. Test embeds
  // without an appId bypass them and render the local shell.
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
      <div className="h-screen flex items-center justify-center bg-paper p-6">
        <div data-testid="workspace-error" className="text-center max-w-md">
          <h1 className="font-serif italic text-2xl text-ink mb-2">Project not found</h1>
          <p className="font-serif text-ink-soft mb-4">
            We couldn't load that project. It may have been deleted or you don't have access.
          </p>
          <Link
            to="/home"
            data-testid="workspace-error-home"
            className="font-serif italic text-tomato hover:opacity-80 focus:outline-2 focus:outline-tomato focus:outline-offset-2 rounded-sm"
          >
            ← Back to home
          </Link>
        </div>
      </div>
    );
  }

  const projectName = appQuery.data?.name ?? projectNameProp ?? "untitled";

  if (hasInvalidCanvasPath) {
    return (
      <div className="h-screen flex items-center justify-center bg-paper p-6">
        <div className="text-center max-w-md">
          <h1 className="font-serif italic text-2xl text-ink mb-2">Canvas not found</h1>
          <p className="font-serif text-ink-soft mb-4">
            That workspace view does not exist.
          </p>
          <button
            type="button"
            onClick={() => setActive("preview")}
            className="font-serif italic text-tomato hover:opacity-80 bg-transparent border-0 cursor-pointer"
          >
            Back to preview
          </button>
        </div>
      </div>
    );
  }

  function setActive(next: CanvasPillId) {
    setActiveState(next);
    if (!appId) return;
    navigate(`/p/${encodeURIComponent(appId)}/${next}`, { replace: false });
  }

  return (
    <div className="h-screen flex flex-col bg-paper">
      <TopBar
        projectName={projectName}
        center={
          <div className="flex items-center gap-3">
            <CanvasPills active={active} onChange={setActive} tier={tier} />
            <TierToggle tier={tier} onChange={setTier} />
          </div>
        }
        right={
          <div className="flex items-center gap-2">
            {isPhone && (
              <button
                type="button"
                onClick={() => setChatOpen((v) => !v)}
                data-testid="topbar-chat-toggle"
                aria-label={chatOpen ? "Close chat" : "Open chat"}
                aria-expanded={chatOpen}
                title={chatOpen ? "Close chat" : "Open chat"}
                className="inline-flex items-center justify-center size-7 border border-rule rounded-full bg-paper-2 font-serif italic text-[13px] text-ink-soft hover:border-ink hover:text-ink cursor-pointer focus:outline-2 focus:outline-tomato focus:outline-offset-2"
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
              className="inline-flex items-center justify-center size-7 border border-rule rounded-full bg-paper-2 font-serif italic text-[13px] text-ink-soft hover:border-ink hover:text-ink cursor-pointer focus:outline-2 focus:outline-tomato focus:outline-offset-2"
            >
              ?
            </button>
            <a
              href="#"
              data-testid="topbar-url"
              aria-label={`Live URL: ${projectName}.zeroship.app`}
              className="hidden sm:inline-flex items-center gap-2 px-3 py-1.5 border border-rule rounded-full bg-paper-2 font-mono text-[11px] text-ink-soft hover:border-ink hover:text-ink focus:outline-2 focus:outline-tomato focus:outline-offset-2"
              style={{ textDecoration: "none" }}
            >
              <span className="size-[5px] rounded-full bg-ivy pulse-dot" aria-hidden="true" />
              {projectName}.zeroship.app
            </a>
          </div>
        }
        accountInitials="ZS"
      />

      <div
        className="flex-1 grid min-h-0"
        style={{
          // On phones, the chat collapses out of the layout entirely
          // and re-mounts as a drawer below; the main column claims
          // 100% of the width.
          gridTemplateColumns: isPhone ? "1fr" : "1fr 320px",
        }}
      >
        <main data-testid="canvas-area" className="min-h-0 min-w-0 overflow-y-auto flex flex-col">
          {showLiveBanner && appId && appQuery.data && (
            <LiveBanner
              appName={appQuery.data.name}
              appUrl={`${appQuery.data.name}.zeroship.app`}
              shippedAgo="just now"
              onDismiss={() => setShowLiveBanner(false)}
            />
          )}
          {/* Each canvas is wrapped in its own ErrorBoundary so a render
              crash in one pane does not blank the whole workspace.
              The fallback at the bottom keeps each pill clickable
              when an appId is not available. */}
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
            <div className="h-full flex items-center justify-center text-ink-soft font-serif italic px-6 text-center">
              No project selected.
            </div>
          )}
        </main>
        {!isPhone && (
          <aside className="border-l border-rule min-h-0 min-w-0 overflow-hidden flex flex-col">
            <ErrorBoundary label="the chat rail">
              <ChatRail
                appName={projectName}
                appId={appId}
                seedBrief={seedBrief ?? undefined}
              />
            </ErrorBoundary>
          </aside>
        )}
      </div>

      {/* Phone drawer: full-screen chat overlay. Mounted only while
          open so the rail's onMount seedBrief effect fires correctly,
          and a backdrop-click closes it. */}
      {isPhone && chatOpen && (
        <div
          data-testid="chat-drawer"
          role="dialog"
          aria-modal="true"
          aria-label="Chat"
          className="fixed inset-0 z-40 flex flex-col bg-paper-2"
        >
          <div className="flex items-center justify-between px-4 py-2 border-b border-rule bg-paper">
            <span className="font-display italic font-medium text-base">Notes &amp; thoughts</span>
            <button
              type="button"
              onClick={() => setChatOpen(false)}
              data-testid="chat-drawer-close"
              aria-label="Close chat"
              className="font-serif italic text-[13px] text-ink-soft hover:text-ink bg-transparent border-0 cursor-pointer px-2 py-1 focus:outline-2 focus:outline-tomato focus:outline-offset-2"
            >
              close
            </button>
          </div>
          <div className="flex-1 min-h-0">
            <ErrorBoundary label="the chat rail">
              <ChatRail
                appName={projectName}
                appId={appId}
                seedBrief={seedBrief ?? undefined}
              />
            </ErrorBoundary>
          </div>
        </div>
      )}

      <ProductTour open={tourOpen} onClose={() => setTourOpen(false)} />
    </div>
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
      className="hidden sm:inline-flex font-serif italic text-[12px] text-ink-soft hover:text-ink bg-transparent border-0 cursor-pointer pl-1 pr-1 focus:outline-2 focus:outline-tomato focus:outline-offset-2"
    >
      {label}
    </button>
  );
}
