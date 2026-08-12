import React from "react";
import { createRoot } from "react-dom/client";
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
      <App />
    </ThemeProvider>
  </React.StrictMode>,
);
