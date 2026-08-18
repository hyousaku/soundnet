import type { RouteHealth } from "./protocol";

export interface HealthView {
  /** Short label for the health column. */
  text: string;
  /** Longer explanation for a tooltip, when there is more to say. */
  title?: string;
  /** Colour for both the label and, in the patchbay, the edge. */
  color: string;
  /** Whether this route should be drawn as visibly wrong (red row/edge). */
  bad: boolean;
}

export const HEALTH_OK = "#4ade80";
export const HEALTH_BAD = "#ef5350";
/** Amber, matching the "partial latency" warning colour in latency.ts. */
export const HEALTH_STALLED = "#f59e0b";

/**
 * One description of a route's health, shared by the route table and the
 * patchbay edges.
 *
 * Shared rather than written twice because the two views disagreeing about
 * whether a route is healthy is worse than either being wrong: the patchbay
 * is what an operator looks at first, and a green edge over a red row reads
 * as a UI glitch rather than as a problem with the audio.
 */
export function describeHealth(health: RouteHealth | undefined): HealthView {
  if (!health) {
    // No stats for this route at all — this engine has no local role in it,
    // so it has nothing to report. Not the same as healthy.
    return { text: "—", color: "#8b93a1", bad: false };
  }
  switch (health.type) {
    case "retrying":
      return {
        text: `retrying (${health.attempts}) — ${health.reason}`,
        title: health.reason,
        color: HEALTH_BAD,
        bad: true,
      };
    case "stalled": {
      // Name the side, because the two have completely different causes: a
      // capture stall is the interface this machine reads from, a playback
      // stall is the one it writes to, and telling an operator to go check
      // "the device" when there are two is barely better than not telling
      // them at all.
      const sides: string[] = [];
      if (health.capture) sides.push("input");
      if (health.playback) sides.push("output");
      const which = sides.join(" and ") || "device";
      return {
        text: `stalled (${which})`,
        title:
          `The ${which} device has stopped moving audio — no periods in or out for ` +
          `several seconds. The route is still running and will recover on its own ` +
          `if the device does; check that the interface is still plugged in and that ` +
          `nothing else has taken it.`,
        color: HEALTH_STALLED,
        bad: true,
      };
    }
    case "ok":
      return { text: "ok", color: HEALTH_OK, bad: false };
  }
}
