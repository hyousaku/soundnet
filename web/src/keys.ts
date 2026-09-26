/// Whether a key event belongs to whatever has focus rather than to the app.
///
/// Single-letter shortcuts must never fire while someone is typing a host
/// name, and must not fire on a focused `<select>` either: there a letter is
/// type-ahead (pressing "4" on the rate menu jumps to 44.1k), and stealing it
/// would make the menu behave differently depending on which letters happen
/// to be bound.
export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  if (tag === "TEXTAREA" || tag === "SELECT") return true;
  if (tag === "INPUT") {
    const type = (target as HTMLInputElement).type;
    // Checkboxes and buttons take Space, not letters, so shortcuts are fine
    // there. Anything that accepts text is not.
    return !["checkbox", "radio", "button", "submit", "reset", "range"].includes(type);
  }
  return false;
}

/// Everything that can take keyboard focus inside `root`, in tab order.
export function focusables(root: HTMLElement): HTMLElement[] {
  const selector =
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), ' +
    'textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
  return Array.from(root.querySelectorAll<HTMLElement>(selector)).filter(
    (el) => !el.hasAttribute("inert") && el.offsetParent !== null,
  );
}

/// The shortcuts, in one place so the help dialog cannot drift from what the
/// handlers actually do.
export const SHORTCUTS: Array<{ keys: string[]; action: string }> = [
  { keys: ["N"], action: "New route" },
  { keys: ["H"], action: "Add host" },
  { keys: ["R"], action: "Rescan audio devices" },
  { keys: ["J", "K"], action: "Select next / previous route" },
  { keys: ["Del"], action: "Remove selected route (asks first)" },
  { keys: ["Esc"], action: "Close dialog / clear selection" },
  { keys: ["1"], action: "Go to sidebar" },
  { keys: ["2"], action: "Go to patch bay" },
  { keys: ["3"], action: "Go to route table" },
  { keys: ["F"], action: "Fit patch bay to screen" },
  { keys: ["?"], action: "Show this help" },
];

export const CANVAS_SHORTCUTS: Array<{ keys: string[]; action: string }> = [
  { keys: ["←", "↑", "→", "↓"], action: "Pan (with the patch bay itself focused)" },
  { keys: ["+", "−"], action: "Zoom in / out" },
  { keys: ["0"], action: "Fit to screen" },
  { keys: ["Tab"], action: "Move between host cards and wires" },
  { keys: ["Enter"], action: "Select the focused card or wire (on a wire, Del then removes its route)" },
  { keys: ["←", "↑", "→", "↓"], action: "Move the selected host card (with the card focused)" },
];
