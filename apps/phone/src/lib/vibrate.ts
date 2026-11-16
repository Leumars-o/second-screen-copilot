import type { AnswerStart } from "@ssc/protocol";

/** navigator.vibrate patterns (ms on/off) by urgency: stronger and longer as urgency rises. */
export const VIBRATION: Record<AnswerStart["urgency"], number[]> = {
  low: [40],
  medium: [90, 60, 90],
  high: [200, 80, 200, 80, 200],
};

export function vibrationPattern(u: AnswerStart["urgency"]): number[] {
  return VIBRATION[u];
}

export function browserVibrate(pattern: number[]): void {
  if (typeof navigator !== "undefined" && typeof navigator.vibrate === "function") navigator.vibrate(pattern);
}
