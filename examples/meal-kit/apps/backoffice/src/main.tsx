import { createRoot } from "react-dom/client";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import App from "./App";
import "./styles.css";
createRoot(document.getElementById("root")!).render(<BrowserRouter><Routes>
  <Route path="/m/:market/:locale/*" element={<App />} />
  <Route path="*" element={<Navigate replace to="/m/us/en/operations" />} />
</Routes></BrowserRouter>);
