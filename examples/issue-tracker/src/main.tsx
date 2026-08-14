import React from "react";
import { createRoot } from "react-dom/client";
import { AuthProvider } from "@zeroship/auth/react";
import { BrowserRouter } from "react-router-dom";
import { ThemeProvider } from "@zeroship/ui";
// The design system ships its stylesheet as a separate export. Importing only
// the module gets you the components with no chrome at all -- the AppShell
// rendered as a bare stack of links until this line existed.
import "@zeroship/ui/styles.css";
import { App } from "./App";
// After the design system, so the app sheet can override tokens rather than
// be overridden by them.
import "./styles.css";

const el = document.getElementById("root");
if (!el) throw new Error("missing #root");
createRoot(el).render(
  <React.StrictMode>
    <ThemeProvider>
      {/* The app had no way in. Every signed-out state said "sign in through
          the platform and reload" and nothing anywhere performed a sign-in --
          the only route to an identity was minting the dev session cookie by
          hand, which is what the e2e helper does and what no person would.
          The platform ships the flow; the app simply never called it. */}
      <AuthProvider>
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </AuthProvider>
    </ThemeProvider>
  </React.StrictMode>,
);
