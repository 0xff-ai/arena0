import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./styles/app.css";

const root = document.getElementById("root");
if (!root) throw new Error("index.html lost its #root element");

createRoot(root).render(
  <StrictMode>
    <div className="p-4 font-mono text-sm text-muted">arena0</div>
  </StrictMode>,
);
