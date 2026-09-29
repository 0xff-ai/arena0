import { lazy, StrictMode, Suspense } from "react";
import { createRoot } from "react-dom/client";
import "./styles/app.css";

const root = document.getElementById("root");
if (!root) throw new Error("index.html lost its #root element");

// The gallery is a dev-only page; the production bundle never contains it.
const Gallery =
  import.meta.env.DEV && location.pathname === "/gallery"
    ? lazy(() => import("./gallery/Gallery").then((module) => ({ default: module.Gallery })))
    : null;

createRoot(root).render(
  <StrictMode>
    {Gallery ? (
      <Suspense fallback={null}>
        <Gallery />
      </Suspense>
    ) : (
      <div className="p-4 font-mono text-sm text-muted">arena0</div>
    )}
  </StrictMode>,
);
