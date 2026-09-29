import { extendTailwindMerge } from "tailwind-merge";
import { createTV } from "tailwind-variants";

export type { VariantProps } from "tailwind-variants";

// Font sizes from app.css that are not t-shirt sizes. Without them
// tailwind-merge reads `text-code` as a colour and drops it when a real
// colour class (`text-fg`) follows.
const twMergeConfig = { extend: { theme: { text: ["code", "glyph"] } } };

const twMerge = extendTailwindMerge(twMergeConfig);

export const tv = createTV({ twMergeConfig });

/** Joins the truthy class names and lets the last conflicting utility win. */
export function cx(...classes: Array<string | false | null | undefined>): string {
  return twMerge(classes.filter(Boolean).join(" "));
}
