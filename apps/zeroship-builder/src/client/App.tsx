import { BrowserRouter, Routes, Route } from "react-router-dom";
import { WorkspaceShell } from "./workspace/WorkspaceShell";

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route path="*" element={<WorkspaceShell />} />
      </Routes>
    </BrowserRouter>
  );
}
