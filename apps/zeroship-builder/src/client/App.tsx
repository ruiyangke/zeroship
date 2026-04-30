// ─── App — routing root for zeroship-builder ────────────────────
//
// /                        Home
// /templates               Template gallery
// /new[?template=…|prompt=…] 3-step new-project wizard
// /p/:appId                Project workspace shell
//   /preview               default — printed-plate preview
//   /files                 manuscript view
//   /logs                  ledger
//   /env                   key cabinet
//   /settings              project details + danger zone
// /account                 profile + plan + sign out
// /login · /signup         auth
// /admin                   library overview
// /admin/apps · /admin/apps/:id · /admin/users · /admin/revenue · /admin/journal
//   admin surface (gated by role — wiring TODO; visible to authed users today)
//
// Auth state lives in AuthContext. In dev mode (`import.meta.env.DEV`)
// userinfo is bypassed and a synthetic user is provided.

import { BrowserRouter, Routes, Route, Navigate, useLocation } from "react-router-dom";
import type { ReactNode } from "react";

import { AuthProvider, useAuth } from "./auth/AuthContext";
import { Home } from "./pages/Home";
import { Templates } from "./pages/Templates";
import { NewProject } from "./pages/NewProject";
import { Account } from "./pages/Account";
import Login from "./pages/Login";
import Signup from "./pages/Signup";

import { ProjectWorkspace } from "./workspace/ProjectWorkspace";
import { PreviewTab } from "./workspace/tabs/PreviewTab";
import { FilesTab } from "./workspace/tabs/FilesTab";
import { LogsTab } from "./workspace/tabs/LogsTab";
import { EnvTab } from "./workspace/tabs/EnvTab";
import { SettingsTab } from "./workspace/tabs/SettingsTab";

import { AdminLibrary } from "./admin/pages/Library";
import { AdminApps } from "./admin/pages/AdminApps";
import { AdminAppDetail } from "./admin/pages/AdminAppDetail";
import { AdminUsers } from "./admin/pages/AdminUsers";
import { AdminRevenue } from "./admin/pages/AdminRevenue";
import { AdminJournal } from "./admin/pages/AdminJournal";

export default function App() {
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
  return (
    <Routes>
      {/* Auth */}
      <Route path="/login"  element={<RedirectIfAuthed><Login /></RedirectIfAuthed>} />
      <Route path="/signup" element={<RedirectIfAuthed><Signup /></RedirectIfAuthed>} />

      {/* Creator surface */}
      <Route path="/"          element={<RequireAuth><Home /></RequireAuth>} />
      <Route path="/templates" element={<RequireAuth><Templates /></RequireAuth>} />
      <Route path="/new"       element={<RequireAuth><NewProject /></RequireAuth>} />
      <Route path="/account"   element={<RequireAuth><Account onLogout={() => { void logout(); }} /></RequireAuth>} />

      {/* Workspace + tabs */}
      <Route path="/p/:appId" element={<RequireAuth><ProjectWorkspace /></RequireAuth>}>
        <Route index             element={<Navigate to="preview" replace />} />
        <Route path="preview"    element={<PreviewTab />} />
        <Route path="files"      element={<FilesTab />} />
        <Route path="logs"       element={<LogsTab />} />
        <Route path="env"        element={<EnvTab />} />
        <Route path="settings"   element={<SettingsTab />} />
      </Route>

      {/* Admin surface */}
      <Route path="/admin"            element={<RequireAuth><AdminLibrary /></RequireAuth>} />
      <Route path="/admin/apps"       element={<RequireAuth><AdminApps /></RequireAuth>} />
      <Route path="/admin/apps/:appId" element={<RequireAuth><AdminAppDetail /></RequireAuth>} />
      <Route path="/admin/users"      element={<RequireAuth><AdminUsers /></RequireAuth>} />
      <Route path="/admin/revenue"    element={<RequireAuth><AdminRevenue /></RequireAuth>} />
      <Route path="/admin/journal"    element={<RequireAuth><AdminJournal /></RequireAuth>} />

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
      <div className="min-h-screen flex items-center justify-center font-serif italic text-pencil">
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
