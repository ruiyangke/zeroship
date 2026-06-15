import { Suspense, lazy } from "react";
import { BrowserRouter, Routes, Route } from "react-router-dom";
import { AuthGuard } from "./auth/AuthGuard";
import { DevEventsBadge } from "./components/DevEventsBadge";

const WorkspaceShell = lazy(() =>
  import("./workspace/WorkspaceShell").then((m) => ({ default: m.WorkspaceShell })),
);
const WizardWorkspace = lazy(() =>
  import("./pages/WizardWorkspace").then((m) => ({ default: m.WizardWorkspace })),
);
const Home = lazy(() => import("./pages/Home").then((m) => ({ default: m.Home })));
const Marketing = lazy(() =>
  import("./pages/Marketing").then((m) => ({ default: m.Marketing })),
);
const Pricing = lazy(() => import("./pages/Pricing").then((m) => ({ default: m.Pricing })));
const Skills = lazy(() => import("./pages/Skills").then((m) => ({ default: m.Skills })));
const Templates = lazy(() =>
  import("./pages/Templates").then((m) => ({ default: m.Templates })),
);
const About = lazy(() => import("./pages/About").then((m) => ({ default: m.About })));
const Changelog = lazy(() =>
  import("./pages/Changelog").then((m) => ({ default: m.Changelog })),
);
const Privacy = lazy(() => import("./pages/Privacy").then((m) => ({ default: m.Privacy })));
const Terms = lazy(() => import("./pages/Terms").then((m) => ({ default: m.Terms })));
const Login = lazy(() => import("./pages/Login"));
const Signup = lazy(() => import("./pages/Signup"));
const ForgotPassword = lazy(() => import("./pages/ForgotPassword"));
const Account = lazy(() => import("./pages/Account").then((m) => ({ default: m.Account })));
const OnboardingIntent = lazy(() =>
  import("./pages/OnboardingIntent").then((m) => ({ default: m.OnboardingIntent })),
);
const NotFound = lazy(() => import("./pages/NotFound").then((m) => ({ default: m.NotFound })));

function RouteFallback() {
  return (
    <div
      data-testid="route-loading"
      style={{
        minHeight: "100dvh",
        display: "grid",
        placeItems: "center",
        color: "var(--zs-color-fg-muted)",
        background: "var(--zs-color-bg)",
      }}
    >
      loading…
    </div>
  );
}

export default function App() {
  return (
    <BrowserRouter>
      <Suspense fallback={<RouteFallback />}>
        <Routes>
          {/* ─── Public surfaces (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5) ───────────────────── */}
          {/* `/` is the marketing landing for unauthed visitors. The
              authed gallery (project list) lives at `/home` behind
              AuthGuard. Post-login redirects target `/home`. */}
          <Route path="/" element={<Marketing />} />
          <Route path="/pricing" element={<Pricing />} />
          <Route path="/skills" element={<Skills />} />
          <Route path="/templates" element={<Templates />} />
          <Route path="/about" element={<About />} />
          <Route path="/changelog" element={<Changelog />} />
          <Route path="/legal/privacy" element={<Privacy />} />
          <Route path="/legal/terms" element={<Terms />} />

          {/* Public auth surfaces — no AuthGuard. */}
          <Route path="/login" element={<Login />} />
          <Route path="/signup" element={<Signup />} />
          <Route path="/forgot-password" element={<ForgotPassword />} />

          {/* Onboarding intent picker — first run after signup (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.1).
              Public so the post-signup hand-off doesn't race with auth
              propagation; the page itself doesn't need a session. */}
          <Route path="/onboarding/intent" element={<OnboardingIntent />} />

          {/* Authed gallery — formerly `/`. */}
          <Route path="/home" element={<AuthGuard><Home /></AuthGuard>} />

          {/* Pre-coding clarification flow per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.7 / §4.8.2b.
              PUBLIC by design — the wizard collects a brief without auth,
              and only Begin → createProject triggers a 401 when the user
              isn't signed in. */}
          <Route path="/new" element={<WizardWorkspace />} />

          {/* Project workspace — the WORKSPACE Builder takes over here.
              `:appId` is the typed-id (UUIDv7 + base62) returned by
              createProject; suffix routes (/preview, /files, /env, …) are
              handled inside WorkspaceShell via canvas pills. */}
          <Route
            path="/p/:appId/*"
            element={<AuthGuard><WorkspaceShell /></AuthGuard>}
          />

          {import.meta.env.DEV && (
            <Route
              path="/__test/workspace"
              element={<WorkspaceShell projectName="test project" />}
            />
          )}

          {/* Account settings — gated. */}
          <Route
            path="/account"
            element={<AuthGuard><Account /></AuthGuard>}
          />

          {/* Catch-all stays last so explicit routes match first. */}
          <Route path="*" element={<NotFound />} />
        </Routes>
      </Suspense>
      {/* Dev-only floating analytics inspector. Renders nothing in
          production builds (the component itself short-circuits on
          `import.meta.env.DEV`). Helps verify telemetry firing without
          opening the console. */}
      <DevEventsBadge />
    </BrowserRouter>
  );
}
