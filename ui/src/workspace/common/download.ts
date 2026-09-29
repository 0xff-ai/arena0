/** Save `value` as pretty-printed `<name>.json` through the browser's download. */
export function downloadJson(name: string, value: unknown): void {
  const url = URL.createObjectURL(
    new Blob([JSON.stringify(value, null, 2)], { type: "application/json" }),
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = `${name}.json`;
  link.click();
  // The click has started the download; the object URL is no longer needed.
  URL.revokeObjectURL(url);
}
