// ─── Account — profile + plan + sessions + 2FA + sign out ──────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §6.5. Current sections:
//   · Identity     — name + email (read-only; no edit endpoint yet).
//   · Plan         — free plan + usage + upgrade CTA.
//   · Sessions     — list of active sessions (stubbed for now).
//   · Two-factor   — TOTP enrollment (not wired yet).
//   · Delete       — wipe the account (not wired yet).
//   · Sign out     — calls AuthContext.logout, redirects to /login.
//
// Crystal: built over @zeroship/ui — FormSection (aside layout) for the
// settings sections, Card for the plan/sessions surfaces, Input
// (read-only, combined-shorthand label) for the identity fields, Meter
// for the usage gauge, Button for every action. Bespoke type/panels live
// in the co-located Account.css.

import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import {
  Badge,
  Button,
  Card,
  Cluster,
  FormSection,
  Input,
  Meter,
  Stack,
} from "@zeroship/ui";
import { useAuth } from "../auth/AuthContext";
import { PageFrame } from "../components/PageFrame";
import "./Account.css";

const SESSION_START_KEY = "zeroship_session_started_at";

export function Account({ onLogout }: { onLogout?: () => void }) {
  const { user, logout } = useAuth();
  const navigate = useNavigate();
  const display = user?.name || user?.email || "—";
  const firstWord = display.split(/\s+/)[0];
  const rest = display.split(/\s+/).slice(1).join(" ");

  async function handleLogout() {
    await logout();
    onLogout?.();
    navigate("/login", { replace: true });
  }

  return (
    <PageFrame
      crumb={[{ label: "studio", to: "/home" }, { label: "account" }]}
      maxWidth={760}
      showMarginalia={false}
    >
      <Stack gap={8} data-testid="account-page">
        <header>
          <h1 className="account__name">
            {firstWord}{" "}
            {rest && <span className="account__name-rest">{rest}</span>}
          </h1>
          <div className="account__email">{user?.email ?? "—"}</div>

          <Card variant="surface">
            <Card.Content>
              <Cluster justify="between" align="center" gap={4}>
                <div>
                  <div className="account__plan-name">
                    <em>Free</em> plan
                  </div>
                  <div className="account__plan-usage">
                    412 requests this week · 2 of 3 apps deployed
                  </div>
                  <Meter
                    className="account__plan-meter"
                    value={32}
                    intent="neutral"
                    size="sm"
                    aria-label="Plan usage"
                  />
                </div>
                <Button>Upgrade</Button>
              </Cluster>
            </Card.Content>
          </Card>
        </header>

        <FormSection
          orientation="aside"
          title="Identity"
          description="Read-only for now — name + email come from your sign-up."
        >
          <Input
            label="Display name"
            value={user?.name ?? ""}
            readOnly
            data-testid="account-name"
          />
          <Input
            label="Email"
            value={user?.email ?? ""}
            readOnly
            data-testid="account-email"
          />
        </FormSection>

        <SessionsSection />

        <DeferredSection
          title="Two-factor auth"
          helper="Add a second step (TOTP) to keep your projects safe."
          pendingMessage="TOTP enrollment and recovery codes are not wired yet."
          testId="account-2fa"
        />

        <DeferredSection
          title="Delete account"
          helper="Wipe your projects, sessions, and identity for good."
          pendingMessage="Self-serve account deletion is not wired yet."
          testId="account-delete"
          tone="danger"
        />

        <FormSection
          orientation="aside"
          title={<span className="account__danger-title">Sign out</span>}
        >
          <div>
            <Button
              variant="plain"
              intent="destructive"
              onClick={handleLogout}
              data-testid="account-logout"
            >
              Sign out of zeroship
            </Button>
          </div>
        </FormSection>
      </Stack>
    </PageFrame>
  );
}

/**
 * Sessions block — V1 client-only view of "this browser's session".
 * Reads (or seeds) a localStorage timestamp for the start of the
 * current session and renders a single-row card per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §6.5. The
 * "sign out everywhere" affordance is wired to the same logout path
 * AuthContext exposes (in dev that's a no-op; in prod it'll revoke
 * once the control-plane sessions endpoint lands and the SDK switches to
 * the real list). Self-revoke shouldn't actually do anything in dev —
 * we surface it as "sign out" since that's all we can do client-side.
 */
function SessionsSection() {
  const { logout } = useAuth();
  const navigate = useNavigate();

  // Synthetic session start timestamp. Stable across page reloads so
  // "signed in 5m ago" stays accurate; resets on logout (cleared by
  // the logout handler so the next login mints a fresh start).
  const [startedAt, setStartedAt] = useState<string | null>(null);
  useEffect(() => {
    let v: string | null = null;
    try { v = localStorage.getItem(SESSION_START_KEY); } catch {}
    if (!v) {
      v = new Date().toISOString();
      try { localStorage.setItem(SESSION_START_KEY, v); } catch {}
    }
    setStartedAt(v);
  }, []);

  const ua = useMemo(() => {
    if (typeof navigator === "undefined") return "Browser";
    return shortUserAgent(navigator.userAgent);
  }, []);

  async function handleRevokeAll() {
    try { localStorage.removeItem(SESSION_START_KEY); } catch {}
    await logout();
    navigate("/login", { replace: true });
  }

  return (
    <FormSection
      orientation="aside"
      title="Sessions"
      description="Where you're signed in — V1 shows this browser only."
    >
      <Card variant="outline" data-testid="account-sessions">
        <Card.Content>
          <Stack gap={3}>
            <Cluster justify="between" align="start" gap={3}>
              <div>
                <div className="account__session-title">
                  {ua} · this browser
                </div>
                <div className="account__session-sub">
                  Signed in {startedAt ? relativeTime(startedAt) : "just now"}.
                </div>
              </div>
              <Badge
                intent="success"
                variant="soft"
                size="sm"
                data-testid="account-sessions-current-pill"
              >
                current
              </Badge>
            </Cluster>
            <div>
              <Button
                variant="plain"
                size="small"
                onClick={handleRevokeAll}
                data-testid="account-sessions-revoke-all"
                endSlot={<span aria-hidden="true">→</span>}
              >
                Sign out everywhere
              </Button>
            </div>
            <div className="account__session-note">
              Extra devices will appear here once the control plane exposes
              a real sessions list.
            </div>
          </Stack>
        </Card.Content>
      </Card>
    </FormSection>
  );
}

function shortUserAgent(ua: string): string {
  // Tiny matcher — enough to label "Chrome on macOS" without dragging
  // in a real ua-parser dep. We only show this as flavour; if every
  // matcher misses we just show "Browser" rather than the raw string.
  const browser =
    /Firefox\/(\d+)/.test(ua) ? "Firefox" :
    /Edg\//.test(ua) ? "Edge" :
    /Chrome\//.test(ua) ? "Chrome" :
    /Safari\//.test(ua) ? "Safari" : "Browser";
  const os =
    /Mac OS X/.test(ua) ? "macOS" :
    /Windows NT/.test(ua) ? "Windows" :
    /X11.*Linux/.test(ua) ? "Linux" :
    /Android/.test(ua) ? "Android" :
    /iPhone|iPad/.test(ua) ? "iOS" : "";
  return os ? `${browser} on ${os}` : browser;
}

function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "just now";
  const diff = Date.now() - t;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

function DeferredSection({
  title, helper, pendingMessage, testId, tone,
}: {
  title: string; helper: string; pendingMessage: string; testId: string; tone?: "danger";
}) {
  const heading: ReactNode =
    tone === "danger"
      ? <span className="account__danger-title">{title}</span>
      : title;
  return (
    <FormSection orientation="aside" title={heading} description={helper}>
      <div className="account__deferred" data-testid={testId}>
        Coming soon — {pendingMessage}
      </div>
    </FormSection>
  );
}
