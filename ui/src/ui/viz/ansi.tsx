import type { CSSProperties } from "react";
import { cx } from "../cx";

// Tailwind reads class names from source text, so the 16-colour tables are spelled out.
const FG = [
  "text-ansi-0",
  "text-ansi-1",
  "text-ansi-2",
  "text-ansi-3",
  "text-ansi-4",
  "text-ansi-5",
  "text-ansi-6",
  "text-ansi-7",
  "text-ansi-8",
  "text-ansi-9",
  "text-ansi-10",
  "text-ansi-11",
  "text-ansi-12",
  "text-ansi-13",
  "text-ansi-14",
  "text-ansi-15",
] as const;
const BG = [
  "bg-ansi-0",
  "bg-ansi-1",
  "bg-ansi-2",
  "bg-ansi-3",
  "bg-ansi-4",
  "bg-ansi-5",
  "bg-ansi-6",
  "bg-ansi-7",
  "bg-ansi-8",
  "bg-ansi-9",
  "bg-ansi-10",
  "bg-ansi-11",
  "bg-ansi-12",
  "bg-ansi-13",
  "bg-ansi-14",
  "bg-ansi-15",
] as const;

/** A palette slot (one of the 16 theme colours) or a literal colour from the 256 / true-colour forms. */
type Color = { slot: number } | { css: string };

interface Style {
  fg: Color | null;
  bg: Color | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  inverse: boolean;
}

const PLAIN: Style = {
  fg: null,
  bg: null,
  bold: false,
  dim: false,
  italic: false,
  underline: false,
  inverse: false,
};

// One alternative per kind of sequence, most specific first. Everything that
// matches is removed from the output; only complete `ESC [ params m` with
// digits and semicolons is interpreted (as SGR).
//   1,2  complete CSI: parameter bytes 0x30-0x3F, intermediates 0x20-0x2F, final 0x40-0x7E
//   -    truncated CSI at the end of the text
//   -    OSC, terminated by BEL or ST (or the next ESC / end of text)
//   -    DCS, SOS, PM and APC strings
//   -    any other escape (charset selection, RIS, ...)
//   -    C0 controls except tab and newline, DEL, and the C1 range
const ANSI_SEQUENCE = new RegExp(
  [
    "\\u001b\\[([0-?]*)[ -/]*([@-~])",
    "\\u001b\\[[0-?]*[ -/]*",
    "\\u001b\\][^\\u0007\\u001b]*(?:\\u0007|\\u001b\\\\)?",
    "\\u001b[PX^_][^\\u001b]*(?:\\u001b\\\\)?",
    "\\u001b[ -/]*[0-~]?",
    "[\\u0000-\\u0008\\u000b-\\u001f\\u007f-\\u009f]",
  ].join("|"),
  "g",
);

const cubeLevel = (k: number) => (k === 0 ? 0 : 55 + 40 * k);

function color256(n: number): Color {
  if (n < 16) return { slot: n };
  if (n < 232) {
    const c = n - 16;
    return {
      css: `rgb(${cubeLevel(Math.floor(c / 36))} ${cubeLevel(Math.floor(c / 6) % 6)} ${cubeLevel(c % 6)})`,
    };
  }
  const v = 8 + (n - 232) * 10;
  return { css: `rgb(${v} ${v} ${v})` };
}

const isByte = (n: number | undefined): n is number =>
  n !== undefined && Number.isInteger(n) && n >= 0 && n <= 255;

/** Applies one SGR sequence to `style`; unknown or malformed codes are ignored. */
function applySgr(style: Style, params: string): Style {
  const codes = params === "" ? [0] : params.split(";").map((p) => (p === "" ? 0 : Number(p)));
  let next = { ...style };
  for (let i = 0; i < codes.length; i++) {
    const code = codes[i] ?? 0;
    if (code === 0) next = { ...PLAIN };
    else if (code === 1) next.bold = true;
    else if (code === 2) next.dim = true;
    else if (code === 3) next.italic = true;
    else if (code === 4) next.underline = true;
    else if (code === 7) next.inverse = true;
    else if (code === 22) {
      next.bold = false;
      next.dim = false;
    } else if (code === 23) next.italic = false;
    else if (code === 24) next.underline = false;
    else if (code === 27) next.inverse = false;
    else if (code >= 30 && code <= 37) next.fg = { slot: code - 30 };
    else if (code === 39) next.fg = null;
    else if (code >= 40 && code <= 47) next.bg = { slot: code - 40 };
    else if (code === 49) next.bg = null;
    else if (code >= 90 && code <= 97) next.fg = { slot: code - 90 + 8 };
    else if (code >= 100 && code <= 107) next.bg = { slot: code - 100 + 8 };
    else if (code === 38 || code === 48) {
      const mode = codes[i + 1];
      let color: Color | null = null;
      if (mode === 5) {
        const n = codes[i + 2];
        if (isByte(n)) color = color256(n);
        i += 2;
      } else if (mode === 2) {
        const [r, g, b] = [codes[i + 2], codes[i + 3], codes[i + 4]];
        if (isByte(r) && isByte(g) && isByte(b)) color = { css: `rgb(${r} ${g} ${b})` };
        i += 4;
      }
      if (color) {
        if (code === 38) next.fg = color;
        else next.bg = color;
      }
    }
  }
  return next;
}

interface Run {
  text: string;
  className: string;
  style: CSSProperties | undefined;
}

type Paint = Color | "fg" | "editor" | null;

/** Inverse swaps the two colours; an unset side falls back to the page's own. */
function paints(style: Style): { fg: Paint; bg: Paint } {
  if (style.inverse) return { fg: style.bg ?? "editor", bg: style.fg ?? "fg" };
  return { fg: style.fg, bg: style.bg };
}

function toRun(text: string, style: Style): Run {
  const { fg, bg } = paints(style);
  const css: CSSProperties = {};
  const classes: string[] = [];

  if (fg === "editor") classes.push("text-editor");
  else if (fg === "fg") classes.push("text-fg");
  else if (fg && "slot" in fg) classes.push(FG[fg.slot] ?? "");
  else if (fg) css.color = fg.css;

  if (bg === "editor") classes.push("bg-editor");
  else if (bg === "fg") classes.push("bg-fg");
  else if (bg && "slot" in bg) classes.push(BG[bg.slot] ?? "");
  else if (bg) css.backgroundColor = bg.css;

  if (style.bold) classes.push("font-medium");
  if (style.dim) classes.push("opacity-60");
  if (style.italic) classes.push("italic");
  if (style.underline) classes.push("underline");
  return { text, className: classes.join(" "), style: Object.keys(css).length ? css : undefined };
}

function parse(text: string): Run[] {
  const runs: Run[] = [];
  let style = PLAIN;
  let cursor = 0;
  for (const match of text.matchAll(ANSI_SEQUENCE)) {
    if (match.index > cursor) runs.push(toRun(text.slice(cursor, match.index), style));
    cursor = match.index + match[0].length;
    const [, params, final] = match;
    if (final === "m" && params !== undefined && /^[0-9;]*$/.test(params)) {
      style = applySgr(style, params);
    }
  }
  if (cursor < text.length) runs.push(toRun(text.slice(cursor), style));
  return runs;
}

/**
 * Renders guest-owned terminal text. Only SGR colour and weight sequences are
 * interpreted; every other escape or control sequence is dropped, and the
 * result is React text nodes in spans, never HTML.
 */
export function AnsiText(props: { text: string; className?: string }) {
  return (
    <pre className={cx("m-0 font-mono text-sm leading-5 whitespace-pre text-fg", props.className)}>
      {parse(props.text).map((run, i) => (
        <span key={i} className={run.className || undefined} style={run.style}>
          {run.text}
        </span>
      ))}
    </pre>
  );
}
