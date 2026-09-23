import { createRoot } from "react-dom/client";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import App from "./App";
import { MarketEntry } from "./entry";
import "@gather/meal-kit/styles.css";

createRoot(document.getElementById("root")!).render(
  <BrowserRouter>
    <Routes>
      <Route path="/m/:market/:locale/*" element={<App />} />
      <Route path="/" element={<MarketEntry />} />
      <Route path="*" element={<MarketEntry missing />} />
    </Routes>
  </BrowserRouter>,
);
