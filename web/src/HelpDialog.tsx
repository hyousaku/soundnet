import Dialog from "./Dialog";
import { CANVAS_SHORTCUTS, SHORTCUTS } from "./keys";

function Table({ rows }: { rows: Array<{ keys: string[]; action: string }> }) {
  return (
    <table className="shortcut-table">
      <tbody>
        {rows.map((r) => (
          <tr key={r.action}>
            <td>
              {r.keys.map((k, i) => (
                <span key={k}>
                  {i > 0 && " "}
                  <kbd>{k}</kbd>
                </span>
              ))}
            </td>
            <td>{r.action}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

export default function HelpDialog({ onClose }: { onClose: () => void }) {
  return (
    <Dialog title="Help — mouse and keyboard" onClose={onClose} wide>
      <p className="hint">
        Everything here can also be done with the mouse, and everything the
        mouse can do can be done from the keyboard. Letter shortcuts are
        ignored while you are typing in a field or have a menu focused.
      </p>
      <h4>Anywhere</h4>
      <Table rows={SHORTCUTS} />
      <h4>In the patch bay</h4>
      <Table rows={CANVAS_SHORTCUTS} />
      <h4>In the route table</h4>
      <p className="hint">
        Tab moves between settings; menus change with the arrow keys. Changes
        are applied together about half a second after you stop, or at once
        when you press Enter or leave the row — so stepping through a menu does
        not restart the audio at every step.
      </p>
      <h4>With the mouse</h4>
      <ul className="hint help-list">
        <li>
          Make a route: <b>New route</b> in the top bar, <b>Send…</b> / <b>Play…</b> next
          to a port in the sidebar, or drag from a source’s dot to a
          destination’s dot in the patch bay (clicking one dot and then the
          other works too).
        </li>
        <li>Select a route: click its wire or its row. The two are highlighted together.</li>
        <li>Remove a route: <b>Remove</b> at the end of its row. It always asks first.</li>
        <li>Pan by dragging the background, zoom with the wheel or the +/− buttons.</li>
      </ul>
      <div className="dialog-actions">
        <button type="button" className="primary" onClick={onClose} data-autofocus>
          Close
        </button>
      </div>
    </Dialog>
  );
}
