import { lazy, StrictMode, Suspense } from "react";
import { createRoot } from "react-dom/client";
import { readToken } from "./sync";
import "./styles/app.css";

const root = document.getElementById("root");
if (!root) throw new Error("index.html lost its #root element");

// The gallery is a dev-only page; the production bundle never contains it.
const Gallery =
  import.meta.env.DEV && location.pathname === "/gallery"
    ? lazy(() => import("./gallery/Gallery").then((module) => ({ default: module.Gallery })))
    : null;

// The router snapshots the address when its module loads. Reading the token
// first strips `#token=` from the address bar, so it never enters router
// state; the app module is imported only afterwards.
readToken();
const App = lazy(() => import("./app/App").then((module) => ({ default: module.App })));

createRoot(root).render(
  <StrictMode>
    <Suspense fallback={null}>{Gallery ? <Gallery /> : <App />}</Suspense>
  </StrictMode>,
);
