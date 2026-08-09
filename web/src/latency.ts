// Shared by RouteEditor.tsx (table column) and Patchbay.tsx (edge label) so
// the two views can't drift into disagreeing about what's safe to claim.
//
// The engine deliberately never sends a combined "total latency" figure —
// see the doc comment on StreamStats in crates/soundnet-protocol/src/lib.rs
// for why: a single engine only ever knows its own half of a route (sender:
// ALSA capture buffering; receiver: roc's RTCP-derived e2e figure + ALSA
// playback buffering), and summing just the known half and presenting it as
// "the" latency would read as a real, lower number than the truth. This
// module is where that honesty constraint actually gets enforced in the UI:
// a bare, unlabelled total is only ever produced when every component is
// present.

import type { StreamStats } from "./protocol";

export interface LatencySummary {
  /** Compact text for a table cell or edge label. */
  text: string;
  /** True when `text` is a partial figure, not a genuine end-to-end total. */
  partial: boolean;
  /** Longer explanation, meant for a `title` tooltip. */
  title: string;
}

export function summarizeLatency(s: StreamStats | undefined): LatencySummary {
  if (!s) {
    return { text: "—", partial: false, title: "No data from this engine for this route." };
  }
  const { capture_buffer_ms, roc_e2e_ms, playback_buffer_ms } = s;

  if (capture_buffer_ms != null && roc_e2e_ms != null && playback_buffer_ms != null) {
    // Only reachable for a self-loop route (both ends on this engine) —
    // that's the only way a single engine can hold all three components.
    const total = capture_buffer_ms + roc_e2e_ms + playback_buffer_ms;
    return {
      text: `${total.toFixed(1)}ms total`,
      partial: false,
      title:
        `Full path: capture ${capture_buffer_ms.toFixed(1)}ms + ` +
        `roc e2e ${roc_e2e_ms.toFixed(1)}ms + playback ${playback_buffer_ms.toFixed(1)}ms.`,
    };
  }

  const parts: string[] = [];
  if (capture_buffer_ms != null) parts.push(`cap ${capture_buffer_ms.toFixed(1)}`);
  if (roc_e2e_ms != null) parts.push(`roc ${roc_e2e_ms.toFixed(1)}`);
  if (playback_buffer_ms != null) parts.push(`pb ${playback_buffer_ms.toFixed(1)}`);

  if (parts.length === 0) {
    return {
      text: "—",
      partial: false,
      title: "This engine has no local role in either end of this route.",
    };
  }

  const missing: string[] = [];
  if (capture_buffer_ms == null) missing.push("sender's ALSA capture buffer");
  if (roc_e2e_ms == null) missing.push("roc e2e (needs the receiver's RTCP data)");
  if (playback_buffer_ms == null) missing.push("receiver's ALSA playback buffer");

  return {
    text: `${parts.join("+")}ms partial`,
    partial: true,
    title:
      `Partial — only what this engine can see (${parts.join(", ")}ms). ` +
      `Not included, measured on the other engine: ${missing.join("; ")}.`,
  };
}
