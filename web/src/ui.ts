import { create } from "zustand";
import type { RouteId } from "./protocol";

/// Pre-selection for the "New route" dialog, so it can be opened already
/// pointing at the port the operator was looking at.
export interface RoutePreset {
  src?: string; // "<nodeId>|<portId>"
  dst?: string;
}

/// At most one modal at a time. A stack of dialogs is a thing an operator has
/// to keep track of, and nothing here needs one: every dialog either finishes
/// what it started or is cancelled.
export type DialogState =
  | { kind: "addHost" }
  | { kind: "newRoute"; preset?: RoutePreset }
  | { kind: "help" }
  | {
      kind: "confirm";
      title: string;
      body: string;
      confirmLabel: string;
      /// Destructive actions get a red button *and* start with focus on
      /// Cancel, so a reflexive Enter does the safe thing. At an event, the
      /// cost of an unintended "remove" is somebody's audio going dead.
      danger?: boolean;
      onConfirm: () => void;
    };

/// Browser-only UI state. Kept apart from `store.ts`, which mirrors the
/// engine: nothing here is ever sent anywhere, and mixing the two would make
/// it easy to mistake a selection for something the engine knows about.
interface Ui {
  dialog: DialogState | null;
  openDialog: (d: DialogState) => void;
  closeDialog: () => void;
  /// The route the operator is currently looking at — clicked on the canvas,
  /// or focused in the table. Shared so the two views highlight the same
  /// thing: clicking a wire finds its row, and tabbing through the table
  /// lights up its wire.
  selectedRouteId: RouteId | null;
  selectRoute: (id: RouteId | null) => void;
  /// Registered by the patch bay once React Flow is ready, so a shortcut
  /// handled elsewhere can fit the view without reaching into the canvas.
  fitView?: () => void;
}

export const useUi = create<Ui>((set) => ({
  dialog: null,
  openDialog: (dialog) => set({ dialog }),
  closeDialog: () => set({ dialog: null }),
  selectedRouteId: null,
  selectRoute: (selectedRouteId) => set({ selectedRouteId }),
}));

/// Ask before doing something that cannot be taken back.
export function confirmAction(opts: {
  title: string;
  body: string;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => void;
}): void {
  useUi.getState().openDialog({ kind: "confirm", ...opts });
}

/// Encode a node/port pair as a single `<select>` value. `|` appears in
/// neither: node ids are UUIDs or `manual:<addr>:<port>`, and port ids have
/// every separator ALSA uses replaced with `_` (see `devices.rs::port_id`).
export const portKey = (nodeId: string, portId: string) => `${nodeId}|${portId}`;
export const splitPortKey = (key: string): [string, string] => {
  const i = key.indexOf("|");
  return [key.slice(0, i), key.slice(i + 1)];
};
