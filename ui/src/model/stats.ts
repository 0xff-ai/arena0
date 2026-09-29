import type { StepRow } from "../sync";

/** Below this many intervals a percentile says nothing, so there is none. */
const MIN_SAMPLES = 8;

export interface ProgramStats {
  /** Percentiles of the program's step intervals in ms; null below 8 samples. */
  p75: number | null;
  p95: number | null;
  samples: number;
  /**
   * Every interval in ms, ascending. Flags need the whole distribution to
   * tell how rare a wait is ("top 5%"), which the two percentiles cannot.
   */
  intervals: number[];
}

/**
 * Step intervals per program: the time between successive certified steps of
 * one execution (one Host's copy), pooled over every execution of the program.
 * `programOf` maps an execution key to its program hash; steps of executions
 * it does not know are ignored.
 */
export function programStats(
  steps: StepRow[],
  programOf: Map<string, string>,
): Map<string, ProgramStats> {
  const byExecution = new Map<string, { program: string; rows: StepRow[] }>();
  for (const step of steps) {
    // A step row carries `{host, exec_id}`, which is its execution's key.
    const key = `${step.host}/${step.exec_id}`;
    const program = programOf.get(key);
    if (program === undefined) continue;
    const entry = byExecution.get(key);
    if (entry === undefined) byExecution.set(key, { program, rows: [step] });
    else entry.rows.push(step);
  }

  const intervalsByProgram = new Map<string, number[]>();
  for (const { program, rows } of byExecution.values()) {
    const intervals = intervalsByProgram.get(program) ?? [];
    intervalsByProgram.set(program, intervals);
    rows.sort((a, b) => a.step - b.step);
    let previous: StepRow | undefined;
    for (const row of rows) {
      if (previous !== undefined) intervals.push(row.certified_ms - previous.certified_ms);
      previous = row;
    }
  }

  const stats = new Map<string, ProgramStats>();
  for (const [program, intervals] of intervalsByProgram) {
    intervals.sort((a, b) => a - b);
    const enough = intervals.length >= MIN_SAMPLES;
    stats.set(program, {
      p75: enough ? percentile(intervals, 75) : null,
      p95: enough ? percentile(intervals, 95) : null,
      samples: intervals.length,
      intervals,
    });
  }
  return stats;
}

/** Nearest rank on an ascending, non-empty list. */
function percentile(sorted: number[], p: number): number | null {
  return sorted[Math.ceil((p / 100) * sorted.length) - 1] ?? null;
}
