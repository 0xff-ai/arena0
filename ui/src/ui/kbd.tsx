import { isMac } from "./internal";

const THIN_SPACE = " ";

const macGlyphs: Record<string, string> = {
  Mod: "⌘",
  Ctrl: "⌃",
  Alt: "⌥",
  Shift: "⇧",
};

const commonGlyphs: Record<string, string> = {
  Mod: "Ctrl",
  Enter: "↵",
  Backspace: "⌫",
  Tab: "⇥",
  Up: "↑",
  Down: "↓",
  Left: "←",
  Right: "→",
};

function glyph(key: string): string {
  if (isMac) return macGlyphs[key] ?? commonGlyphs[key] ?? key;
  return commonGlyphs[key] ?? key;
}

/** A shortcut such as "Mod+K": ⌘K on macOS, "Ctrl K" (thin space) elsewhere. */
export function Kbd(props: { keys: string }) {
  const text = props.keys
    .split("+")
    .map(glyph)
    .join(isMac ? "" : THIN_SPACE);
  return (
    <kbd className="inline-flex h-4 shrink-0 items-center rounded-xs border border-line bg-editor px-1 font-sans text-2xs leading-none text-subtle">
      {text}
    </kbd>
  );
}
