import { useUi } from "./ui";
import AddHostDialog from "./AddHostDialog";
import NewRouteDialog from "./NewRouteDialog";
import HelpDialog from "./HelpDialog";
import ConfirmDialog from "./ConfirmDialog";

/// Renders whichever dialog is open. One place, so there is only ever one
/// modal on screen and every way of opening one — a toolbar button, a
/// shortcut, the canvas — goes through the same door.
export default function DialogHost() {
  const dialog = useUi((s) => s.dialog);
  const close = useUi((s) => s.closeDialog);
  if (!dialog) return null;
  switch (dialog.kind) {
    case "addHost":
      return <AddHostDialog onClose={close} />;
    case "newRoute":
      return <NewRouteDialog preset={dialog.preset} onClose={close} />;
    case "help":
      return <HelpDialog onClose={close} />;
    case "confirm":
      return (
        <ConfirmDialog
          title={dialog.title}
          body={dialog.body}
          confirmLabel={dialog.confirmLabel}
          danger={dialog.danger}
          onConfirm={dialog.onConfirm}
          onClose={close}
        />
      );
  }
}
