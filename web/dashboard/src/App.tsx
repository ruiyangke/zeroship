// ─── App — routing root ─────────────────────────────────────────
//
// Route map (post-refactor):
//
//   /                        Home (gallery + prompt)
//   /p/:appId                → /p/:appId/chat
//   /p/:appId/chat           ChatTab (preview + chat rail)
//   /p/:appId/files          FilesTab (CM6 + file tree)
//   /p/:appId/logs           LogsTab
//   /p/:appId/env            EnvTab
//   /p/:appId/settings       SettingsTab
//   /account                 Account
//   /admin/apps              AppList (legacy)
//   /admin/overview          Overview (legacy)
//   /admin/apps/:id          AppDetail (legacy)
//   /admin/apps/new          CreateApp (legacy)
//   /login                   Login (auto-shown when unauthed)
//
// Legacy /apps/* and /builder/* are aliased to the new routes below
// so bookmarks keep working.

import { useEffect, useState } from "react";
import {
  BrowserRouter, Routes, Route, Navigate, useParams,
} from "react-router-dom";

import { Home } from "./pages/Home";
import { Account } from "./pages/Account";
import Login from "./pages/Login";
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
  const [authed, setAuthed] = useState(() => !!localStorage.getItem("zeroship_key"));

  useEffect(() => {
    const handler = () => setAuthed(!!localStorage.getItem("zeroship_key"));
    window.addEventListener("storage", handler);
    return () => window.removeEventListener("storage", handler);
  }, []);

  const handleLogin = () => setAuthed(true);
  const handleLogout = () => {
    localStorage.removeItem("zeroship_key");
    setAuthed(false);
  };

  return (
    <BrowserRouter>
      <Routes>
        {/* Login is its own surface — no top-bar. */}
        <Route
          path="/login"
          element={
            authed
              ? <Navigate to="/" replace />
              : <Login onLogin={handleLogin} />
          }
        />

        {/* Home page (gallery + prompt). */}
        <Route path="/" element={<Home onLogout={handleLogout} />} />

        {/* Project workspace. Tabs are children of the shell. */}
        <Route path="/p/:appId" element={<ProjectWorkspace onLogout={handleLogout} />}>
          <Route index element={<Navigate to="chat" replace />} />
          <Route path="chat"     element={<ChatTab />} />
          <Route path="files"    element={<FilesTab />} />
          <Route path="logs"     element={<LogsTab />} />
          <Route path="env"      element={<EnvTab />} />
          <Route path="settings" element={<SettingsTab />} />
        </Route>

        {/* Account. */}
        <Route path="/account" element={<Account onLogout={handleLogout} />} />

        {/* Legacy admin under /admin/*. Auth-gated via Layout. */}
        <Route
          path="/admin/*"
          element={
            !authed ? (
              <Navigate to="/login" replace />
            ) : (
              <Layout onLogout={handleLogout}>
                <Routes>
                  <Route index               element={<Overview />} />
                  <Route path="overview"     element={<Overview />} />
                  <Route path="apps"         element={<AppList />} />
                  <Route path="apps/new"     element={<CreateApp />} />
                  <Route path="apps/:id"     element={<AppDetail />} />
                  <Route path="*"            element={<Navigate to="apps" replace />} />
                </Routes>
              </Layout>
            )
          }
        />

        {/* Bookmark redirects from the pre-refactor URLs. */}
        <Route path="/builder"               element={<Navigate to="/" replace />} />
        <Route path="/builder/:appId"        element={<RedirectBuilder />} />
        <Route path="/apps"                  element={<Navigate to="/admin/apps" replace />} />
        <Route path="/apps/new"              element={<Navigate to="/admin/apps/new" replace />} />
        <Route path="/apps/:id"              element={<RedirectLegacyApp />} />
        <Route path="/ai"                    element={<Navigate to="/" replace />} />

        {/* Catch-all */}
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </BrowserRouter>
  );
}

function RedirectBuilder() {
  const { appId } = useParams<{ appId: string }>();
  return <Navigate to={appId ? `/p/${appId}/chat` : "/"} replace />;
}

function RedirectLegacyApp() {
  const { id } = useParams<{ id: string }>();
  // Old AppDetail page lives under /admin now, but redirect bookmarks
  // to the new project workspace by default — that's where most users
  // want to land.
  return <Navigate to={id ? `/p/${id}/chat` : "/"} replace />;
}

export default App;
