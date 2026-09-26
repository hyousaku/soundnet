import { useEffect, useId, useRef, type ReactNode } from "react";
import { focusables } from "./keys";

/// A modal that behaves the same way from the keyboard as from the mouse.
///
/// - Focus moves into it on open (to `[data-autofocus]` if something asks,
///   otherwise the first control) and goes back to whatever opened it on
///   close, so a keyboard user is not dropped at the top of the page.
/// - Tab and Shift+Tab stay inside. Without that, tabbing past the last
///   button walks into the patch bay *behind* the dialog, and the next Enter
///   acts on something the operator cannot see.
/// - Escape closes it; so does a click on the backdrop. The backdrop listens
///   for mousedown rather than click, so a text selection dragged out of an
///   input and released over the backdrop does not dismiss the dialog.
export default function Dialog({
  title,
  onClose,
  children,
  wide,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  wide?: boolean;
}) {
  const boxRef = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    const box = boxRef.current;
    if (box) {
      const first =
        box.querySelector<HTMLElement>("[data-autofocus]") ?? focusables(box)[0] ?? box;
      first.focus();
    }
    return () => {
      // The opener may be gone — removing a route removes its Remove button.
      // Focusing a detached element silently does nothing, so check.
      if (opener && opener.isConnected) opener.focus();
    };
  }, []);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      e.preventDefault();
      onClose();
      return;
    }
    if (e.key !== "Tab" || !boxRef.current) return;
    const items = focusables(boxRef.current);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };

  return (
    <div
      className="dialog"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        ref={boxRef}
        className={wide ? "box wide" : "box"}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        onKeyDown={onKeyDown}
      >
        <h3 id={titleId}>{title}</h3>
        {children}
      </div>
    </div>
  );
}
