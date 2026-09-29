import { useRouter } from "@tanstack/react-router";
import { useEffect } from "react";
import { useComposer } from "./composer/store";
import { useActiveDoc, useCloseDoc, useOpenDoc } from "./nav";
import { useSelection } from "./selection";
import { openPalette, toggleInspector, useProblems } from "./shell";

const LISTS: Record<string, string> = {
  s: "/sessions",
  o: "/offers",
  r: "/receipts",
  p: "/programs",
};

// Outside the effect: the effect re-subscribes on every render, and a chord must survive that.
// A mutated property, not a reassigned `let`: the React Compiler mishandles reassigned module variables.
const chord: { at: number | null } = { at: null };

/** `g` then a letter must complete within this long. */
const CHORD_MS = 1000;

function isEditable(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    target.closest("input, textarea, select, [contenteditable=''], [contenteditable='true']") !==
      null
  );
}

/** Whether a dialog, menu or popover is open; Esc belongs to it then. */
function overlayOpen(): boolean {
  return (
    document.querySelector(
      "[role=dialog], [role=alertdialog], [role=menu], [role=listbox], [role=tooltip]",
    ) !== null
  );
}

/**
 * Global shortcuts, mounted once in `Workspace`. Events from editable
 * elements are ignored except `Mod+K` and `Esc`; `Mod+Enter` is left to the
 * form that has focus. Row navigation (arrows, Enter) is the lists' own.
 */
export function useGlobalKeys(): void {
  const router = useRouter();
  const openDoc = useOpenDoc();
  const closeDoc = useCloseDoc();
  const activeDoc = useActiveDoc();
  const composer = useComposer();
  const [selection] = useSelection();
  const problems = useProblems();

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const mod = event.metaKey || event.ctrlKey;
      const pending = chord.at;
      chord.at = null;

      if (mod && event.key.toLowerCase() === "k") {
        event.preventDefault();
        openPalette("all");
        return;
      }
      if (event.key === "Escape") {
        if (composer.calloutKey !== null && !overlayOpen()) composer.close();
        return;
      }
      if (isEditable(event.target)) return;

      if (mod && event.key === ".") {
        event.preventDefault();
        toggleInspector();
        return;
      }
      if (mod && event.key.toLowerCase() === "w") {
        if (activeDoc === null) return;
        event.preventDefault();
        closeDoc(activeDoc);
        return;
      }
      if (event.key === "F8") {
        event.preventDefault();
        const sessions = problems.sessions.map((problem) => problem.session.key);
        if (sessions.length === 0) return;
        const current =
          activeDoc?.kind === "session"
            ? activeDoc.key
            : selection?.kind === "session"
              ? selection.key
              : selection?.kind === "step"
                ? selection.sessionKey
                : null;
        const at = current === null ? -1 : sessions.indexOf(current);
        const step = event.shiftKey ? -1 : 1;
        // From a session that is not a problem, F8 starts at the first and Shift+F8 at the last.
        const next =
          at < 0
            ? event.shiftKey
              ? sessions.length - 1
              : 0
            : (at + step + sessions.length) % sessions.length;
        const key = sessions[next];
        if (key !== undefined) openDoc({ kind: "session", key });
        return;
      }
      if (mod || event.altKey) return;

      if (pending !== null && event.timeStamp - pending < CHORD_MS) {
        const path = LISTS[event.key];
        if (path !== undefined) {
          event.preventDefault();
          router.history.push(path);
        }
        return;
      }
      if (event.key === "g") {
        chord.at = event.timeStamp;
        return;
      }
      if (event.key === "/") {
        const find = document.querySelector<HTMLInputElement>(
          "[data-region=document] input[type=search]",
        );
        if (find === null) return;
        event.preventDefault();
        find.focus();
        return;
      }
      if (event.key === "j" || event.key === "k") {
        // A list handles ArrowDown/ArrowUp itself; j/k replay them on the focused row.
        const target = event.target;
        if (!(target instanceof HTMLElement)) return;
        if (target.closest("[role=grid], [role=treegrid], [role=listbox]") === null) return;
        event.preventDefault();
        target.dispatchEvent(
          new KeyboardEvent("keydown", {
            key: event.key === "j" ? "ArrowDown" : "ArrowUp",
            bubbles: true,
            cancelable: true,
          }),
        );
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });
}
