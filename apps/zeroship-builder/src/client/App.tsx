// ─── App — routing root for zeroship-builder ───────────────────
//
// Every route runs against this app's own server functions
// (`"use server"` modules in src/server/) — there is no separate
// agent process or dashboard backend. The app IS the dashboard +
// the agent + the auth proxy + the sandbox proxy, deployed as a
// single zeroship app (dogfooded on the platform's own runtime).

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
  const handleLogout = async () => { await logout(); };

  return (
    <Routes>
      <Route path="/login"  element={<RedirectIfAuthed><Login /></RedirectIfAuthed>} />
      <Route path="/signup" element={<RedirectIfAuthed><Signup /></RedirectIfAuthed>} />

      <Route path="/" element={<RequireAuth><Home onLogout={handleLogout} /></RequireAuth>} />

      <Route
        path="/p/:appId"
        element={<RequireAuth><ProjectWorkspace onLogout={handleLogout} /></RequireAuth>}
      >
        <Route index           element={<Navigate to="chat" replace />} />
        <Route path="chat"     element={<ChatTab />} />
        <Route path="files"    element={<FilesTab />} />
        <Route path="logs"     element={<LogsTab />} />
        <Route path="env"      element={<EnvTab />} />
        <Route path="settings" element={<SettingsTab />} />
      </Route>

      <Route path="/account" element={<RequireAuth><Account onLogout={handleLogout} /></RequireAuth>} />

      <Route path="/builder"        element={<Navigate to="/" replace />} />
      <Route path="/builder/:appId" element={<RedirectBuilder />} />

      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

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

export default App;
