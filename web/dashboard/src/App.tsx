import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import { useState, useEffect } from "react";
import Layout from "./components/Layout";
import Login from "./pages/Login";
import Overview from "./pages/Overview";
import AppList from "./pages/AppList";
import AppDetail from "./pages/AppDetail";
import CreateApp from "./pages/CreateApp";

function App() {
  const [authed, setAuthed] = useState(() => !!localStorage.getItem("appbase_key"));

  useEffect(() => {
    const handler = () => setAuthed(!!localStorage.getItem("appbase_key"));
    window.addEventListener("storage", handler);
    return () => window.removeEventListener("storage", handler);
  }, []);

  const handleLogin = () => setAuthed(true);
  const handleLogout = () => {
    localStorage.removeItem("appbase_key");
    setAuthed(false);
  };

  if (!authed) {
    return (
      <BrowserRouter>
        <Login onLogin={handleLogin} />
      </BrowserRouter>
    );
  }

  return (
    <BrowserRouter>
      <Layout onLogout={handleLogout}>
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/apps" element={<AppList />} />
          <Route path="/apps/new" element={<CreateApp />} />
          <Route path="/apps/:id" element={<AppDetail />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </Layout>
    </BrowserRouter>
  );
}

export default App;
