import { BrowserRouter, Routes, Route } from "react-router-dom";
import { WorkspaceShell } from "./workspace/WorkspaceShell";
import { WizardWorkspace } from "./pages/WizardWorkspace";

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        {/* Pre-coding clarification flow per spec §8.2.7 / §4.8.2b.
            Lives outside the workspace shell because no project exists
            yet — wizard runs as a standalone page until "Begin" is
            clicked, then navigates to /p/:appId/preview. */}
        <Route path="/new" element={<WizardWorkspace />} />
        <Route path="*" element={<WorkspaceShell />} />
      </Routes>
    </BrowserRouter>
  );
}
