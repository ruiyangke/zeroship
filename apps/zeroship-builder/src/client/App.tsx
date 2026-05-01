import { BrowserRouter, Routes, Route } from "react-router-dom";
import { WorkspaceShell } from "./workspace/WorkspaceShell";
import { WizardWorkspace } from "./pages/WizardWorkspace";
import { Home } from "./pages/Home";
import Login from "./pages/Login";
import Signup from "./pages/Signup";
import ForgotPassword from "./pages/ForgotPassword";
import { Account } from "./pages/Account";
import { AuthGuard } from "./auth/AuthGuard";

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        {/* Public auth surfaces — no AuthGuard. */}
        <Route path="/login" element={<Login />} />
        <Route path="/signup" element={<Signup />} />
        <Route path="/forgot-password" element={<ForgotPassword />} />

        {/* Landing page — gated. Gallery is for authed creators; an
            unauthed visitor lands on /login (and a marketing page can
            slot in later above the guard). */}
        <Route path="/" element={<AuthGuard><Home /></AuthGuard>} />

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

        {/* Catch-all stays last so /, /new, /p/:appId match first. */}
        <Route path="*" element={<AuthGuard><WorkspaceShell /></AuthGuard>} />
      </Routes>
    </BrowserRouter>
  );
}
