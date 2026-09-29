const pad2 = (n: number) => String(n).padStart(2, "0");

/** "0s", "41s", "8m 45s", "2h 03m", "3d 4h". Negative durations (clock skew) read as "0s". */
export function fmtDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${pad2(s % 60)}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${pad2(m % 60)}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** "20s ago". */
export function fmtAgo(ms: number, now: number): string {
  return `${fmtDuration(now - ms)} ago`;
}

/** Local wall-clock time, "14:03:07". */
export function fmtClock(ms: number): string {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/** 1024-based: "72 B", "1.4 KB", "3.0 MB". */
export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const kb = Math.round((n / 1024) * 10) / 10;
  // Rounding can reach 1024.0: that is 1.0 MB.
  if (kb < 1024) return `${kb.toFixed(1)} KB`;
  return `${(n / 1024 ** 2).toFixed(1)} MB`;
}

export function shortHash(hex: string, n = 8): string {
  return hex.slice(0, n);
}
