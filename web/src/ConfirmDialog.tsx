import Dialog from "./Dialog";

export default function ConfirmDialog({
  title,
  body,
  confirmLabel,
  danger,
  onConfirm,
  onClose,
}: {
  title: string;
  body: string;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}) {
  return (
    <Dialog title={title} onClose={onClose}>
      <p className="dialog-body">{body}</p>
      <div className="dialog-actions">
        {/*
          For a destructive action, focus starts on Cancel. The action is one
          Tab away for anyone who means it, and a reflexive Enter — the key a
          keyboard user has just been pressing to get here — does nothing
          harmful.
        */}
        <button type="button" onClick={onClose} data-autofocus={danger ? true : undefined}>
          Cancel
        </button>
        <button
          type="button"
          className={danger ? "danger" : "primary"}
          data-autofocus={danger ? undefined : true}
          onClick={() => {
            onClose();
            onConfirm();
          }}
        >
          {confirmLabel}
        </button>
      </div>
    </Dialog>
  );
}
