import { twMerge } from "tailwind-merge";

export { tv, type VariantProps } from "tailwind-variants";

/** Joins the truthy class names and lets the last conflicting utility win. */
export function cx(...classes: Array<string | false | null | undefined>): string {
  return twMerge(classes.filter(Boolean).join(" "));
}
