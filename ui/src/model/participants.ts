export type ParticipantTone = "p0" | "p1" | "p2" | "p3" | "p4" | "pq";

const TONES: readonly ParticipantTone[] = ["p0", "p1", "p2", "p3", "p4"];

/** P0 to P4 have their own tone; P5 and up share the grey `pq`. */
export function participantTone(index: number): ParticipantTone {
  return TONES[index] ?? "pq";
}

export function participantLabel(index: number): string {
  return `P${index}`;
}
