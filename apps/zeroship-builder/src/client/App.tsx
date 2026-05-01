import { BrowserRouter, Routes, Route } from "react-router-dom";
import { WorkspaceShell } from "./workspace/WorkspaceShell";
import { WizardWorkspace } from "./pages/WizardWorkspace";
import { Home } from "./pages/Home";
import { Marketing } from "./pages/Marketing";
import { Pricing } from "./pages/Pricing";
import { Skills } from "./pages/Skills";
import { Templates } from "./pages/Templates";
import { About } from "./pages/About";
import { Changelog } from "./pages/Changelog";
import { Privacy } from "./pages/Privacy";
import { Terms } from "./pages/Terms";
import Login from "./pages/Login";
import Signup from "./pages/Signup";
import ForgotPassword from "./pages/ForgotPassword";
import { Account } from "./pages/Account";
import { OnboardingIntent } from "./pages/OnboardingIntent";
import { AuthGuard } from "./auth/AuthGuard";

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        {/* ─── Public surfaces (per spec §5) ───────────────────── */}
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

        {/* Onboarding intent picker — first run after signup (spec §7.1).
            Public so the post-signup hand-off doesn't race with auth
            propagation; the page itself doesn't need a session. */}
        <Route path="/onboarding/intent" element={<OnboardingIntent />} />

        {/* Authed gallery — formerly `/`. */}
        <Route path="/home" element={<AuthGuard><Home /></AuthGuard>} />

        {/* Pre-coding clarification flow per spec §8.2.7 / §4.8.2b.
            PUBLIC by design — the wizard collects a brief without auth,
            and only Begin → createApp triggers a 401 when the user
            isn't signed in. */}
        <Route path="/new" element={<WizardWorkspace />} />

        {/* Project workspace — the WORKSPACE Builder takes over here.
            `:appId` is the typed-id (UUIDv7 + base62) returned by
            createApp; suffix routes (/preview, /code, /env, …) are
            handled inside WorkspaceShell via canvas pills. */}
        <Route
          path="/p/:appId/*"
          element={<AuthGuard><WorkspaceShell /></AuthGuard>}
        />

        {/* Account settings — gated. */}
        <Route
          path="/account"
          element={<AuthGuard><Account /></AuthGuard>}
        />

        {/* Catch-all stays last so explicit routes match first. */}
        <Route path="*" element={<AuthGuard><WorkspaceShell /></AuthGuard>} />
      </Routes>
    </BrowserRouter>
  );
}
