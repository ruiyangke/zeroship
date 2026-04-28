// ─── App — routing root ─────────────────────────────────────────
//
// Route map:
//
//   /                        Home (gallery + prompt) — auth required
//   /p/:appId/{chat|files|logs|env|settings}        — auth required
//   /account                 Account — auth required
//   /admin/*                 Legacy admin pages — auth required
//   /login                   Login (auto-redirects if authed)
//   /signup                  Signup (auto-redirects if authed)
//   Bookmark redirects: /builder, /apps/* → new routes
//
// Auth state lives in AuthContext (src/auth/AuthContext.tsx). In
// dev mode (`import.meta.env.DEV`) we short-circuit to a synthetic
// user — no login screen ever appears. Prod gates everything except
// /login + /signup.

import {
  BrowserRouter, Routes, Route, Navigate, useParams, useLocation,
} from "react-router-dom";
import type { ReactNode } from "react";

import { AuthProvider, useAuth } from "./auth/AuthContext";
import { Home } from "./pages/Home";
import { Account } from "./pages/Account";
import Login from "./pages/Login";
import Signup from "./pages/Signup";
import { ProjectWorkspace } from "./workspace/ProjectWorkspace";
import { ChatTab } from "./workspace/tabs/ChatTab";
import { FilesTab } from "./workspace/tabs/FilesTab";
import { LogsTab } from "./workspace/tabs/LogsTab";
import { EnvTab } from "./workspace/tabs/EnvTab";
import { SettingsTab } from "./workspace/tabs/SettingsTab";

// Legacy admin pages — kept under /admin/* for the platform operator.
import Layout from "./components/Layout";
import Overview from "./pages/Overview";
import AppList from "./pages/AppList";
import AppDetail from "./pages/AppDetail";
import CreateApp from "./pages/CreateApp";

function App() {
  return (
    <BrowserRouter>
      <AuthProvider>
        <AppRoutes />
      </AuthProvider>
    </BrowserRouter>
  );
}

function AppRoutes() {
  const { logout } = useAuth();
  const handleLogout = async () => {
    await logout();
  };

  return (
    <Routes>
      {/* Auth pages. */}
      <Route path="/login"  element={<RedirectIfAuthed><Login /></RedirectIfAuthed>} />
      <Route path="/signup" element={<RedirectIfAuthed><Signup /></RedirectIfAuthed>} />

      {/* Home page (gallery + prompt). */}
      <Route path="/"
        element={<RequireAuth><Home onLogout={handleLogout} /></RequireAuth>} />

      {/* Project workspace. Tabs are children of the shell. */}
      <Route path="/p/:appId"
        element={<RequireAuth><ProjectWorkspace onLogout={handleLogout} /></RequireAuth>}>
        <Route index element={<Navigate to="chat" replace />} />
        <Route path="chat"     element={<ChatTab />} />
        <Route path="files"    element={<FilesTab />} />
        <Route path="logs"     element={<LogsTab />} />
        <Route path="env"      element={<EnvTab />} />
        <Route path="settings" element={<SettingsTab />} />
      </Route>

      {/* Account. */}
      <Route path="/account"
        element={<RequireAuth><Account onLogout={handleLogout} /></RequireAuth>} />

      {/* Legacy admin under /admin/*. */}
      <Route path="/admin/*"
        element={
          <RequireAuth>
            <Layout onLogout={handleLogout}>
              <Routes>
                <Route index           element={<Overview />} />
                <Route path="overview" element={<Overview />} />
                <Route path="apps"     element={<AppList />} />
                <Route path="apps/new" element={<CreateApp />} />
                <Route path="apps/:id" element={<AppDetail />} />
                <Route path="*"        element={<Navigate to="apps" replace />} />
              </Routes>
            </Layout>
          </RequireAuth>
        }
      />

      {/* Bookmark redirects from the pre-refactor URLs. */}
      <Route path="/builder"        element={<Navigate to="/" replace />} />
      <Route path="/builder/:appId" element={<RedirectBuilder />} />
      <Route path="/apps"           element={<Navigate to="/admin/apps" replace />} />
      <Route path="/apps/new"       element={<Navigate to="/admin/apps/new" replace />} />
      <Route path="/apps/:id"       element={<RedirectLegacyApp />} />
      <Route path="/ai"             element={<Navigate to="/" replace />} />

      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

/** Block protected routes until auth resolves; redirect to /login
 *  with a `?return=` carry so we land back here after sign-in. */
function RequireAuth({ children }: { children: ReactNode }) {
  const { user, loading, devBypass } = useAuth();
  const location = useLocation();

  if (devBypass) return <>{children}</>;
  if (loading) {
    return (
      <div className="min-h-screen flex items-center justify-center text-xs text-muted-foreground">
        loading…
      </div>
    );
  }
  if (!user) {
    const ret = encodeURIComponent(location.pathname + location.search);
    return <Navigate to={`/login?return=${ret}`} replace />;
  }
  return <>{children}</>;
}

/** Bounce already-authed users away from /login + /signup.
 *  Note: dev mode does NOT auto-redirect — /login and /signup render
 *  so devs can iterate on the auth UI even with the implicit dev
 *  user in the background. The auth call from the form still works
 *  in dev (control plane runs with --dev-insecure). */
function RedirectIfAuthed({ children }: { children: ReactNode }) {
  const { user, loading, devBypass } = useAuth();
  const location = useLocation();

  if (devBypass) return <>{children}</>;
  if (loading) return null;
  if (user) {
    const params = new URLSearchParams(location.search);
    const ret = params.get("return") ?? "/";
    return <Navigate to={ret} replace />;
  }
  return <>{children}</>;
}

function RedirectBuilder() {
  const { appId } = useParams<{ appId: string }>();
  return <Navigate to={appId ? `/p/${appId}/chat` : "/"} replace />;
}

function RedirectLegacyApp() {
  const { id } = useParams<{ id: string }>();
  return <Navigate to={id ? `/p/${id}/chat` : "/"} replace />;
}

export default App;
