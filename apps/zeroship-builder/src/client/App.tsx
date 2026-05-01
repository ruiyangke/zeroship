import { BrowserRouter, Routes, Route } from "react-router-dom";
import { WorkspaceShell } from "./workspace/WorkspaceShell";
import { WizardWorkspace } from "./pages/WizardWorkspace";
import { Home } from "./pages/Home";

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        {/* Landing page — project gallery + new-project prompt. */}
        <Route path="/" element={<Home />} />
        {/* Pre-coding clarification flow per spec §8.2.7 / §4.8.2b.
            Lives outside the workspace shell because no project exists
            yet — wizard runs as a standalone page until "Begin" is
            clicked, then navigates to /p/:appId/preview. */}
        <Route path="/new" element={<WizardWorkspace />} />
        {/* Project workspace — the WORKSPACE Builder takes over here.
            `:appId` is the typed-id (UUIDv7 + base62) returned by
            createApp; suffix routes (/preview, /code, /env, …) are
            handled inside WorkspaceShell via canvas pills. */}
        <Route path="/p/:appId/*" element={<WorkspaceShell />} />
        {/* Catch-all stays last so /, /new, /p/:appId match first. */}
        <Route path="*" element={<WorkspaceShell />} />
      </Routes>
    </BrowserRouter>
  );
}
