import { Handle, Position, type Node as RFNode, type NodeProps } from "@xyflow/react";
import type { LocalPort, Node as PeerNode } from "./protocol";

export interface PortNodeData extends Record<string, unknown> {
  node: PeerNode;
  ports: LocalPort[];
}

export type PortRFNode = RFNode<PortNodeData, "port">;

/// Node card: hostname header + a row per port, each with its own handle so
/// the user can wire a specific source port to a specific playback port.
/// Capture/Tone rows expose a source handle on the right;
/// Playback rows expose a target handle on the left.
export default function PortNode({ data }: NodeProps<PortRFNode>) {
  const { node, ports } = data;
  const capturePorts = ports.filter((p) => p.kind !== "playback");
  const playbackPorts = ports.filter((p) => p.kind === "playback");

  return (
    <div style={{
      background: "#1c222b",
      color: "#e6e9ef",
      border: "1px solid #262d38",
      borderRadius: 8,
      width: 320,
      overflow: "hidden",
      fontSize: 12,
    }}>
      <div style={{
        padding: "8px 12px",
        background: "#151920",
        borderBottom: "1px solid #262d38",
      }}>
        <div style={{ fontWeight: 600, fontSize: 13 }}>{node.hostname}</div>
        <div style={{ color: "#8a94a5", fontSize: 11 }}>
          {node.addr}:{node.port} · audio :{node.audio_port}
        </div>
      </div>

      <div style={{ padding: "6px 0" }}>
        {capturePorts.length > 0 && (
          <div style={{
            padding: "2px 12px",
            color: "#8a94a5",
            fontSize: 10,
            textTransform: "uppercase",
            letterSpacing: "0.08em",
          }}>Outputs</div>
        )}
        {capturePorts.map((p) => (
          <PortRow key={p.id} port={p} side="source" />
        ))}

        {playbackPorts.length > 0 && (
          <div style={{
            padding: "6px 12px 2px",
            color: "#8a94a5",
            fontSize: 10,
            textTransform: "uppercase",
            letterSpacing: "0.08em",
          }}>Inputs</div>
        )}
        {playbackPorts.map((p) => (
          <PortRow key={p.id} port={p} side="target" />
        ))}
      </div>
    </div>
  );
}

function PortRow({ port, side }: { port: LocalPort; side: "source" | "target" }) {
  const isSource = side === "source";
  return (
    <div style={{
      position: "relative",
      padding: "4px 20px",
      display: "flex",
      justifyContent: isSource ? "flex-end" : "flex-start",
      alignItems: "center",
      gap: 6,
    }}>
      <span style={{
        color: port.kind === "tone" ? "#f59e0b" : (isSource ? "#6cf" : "#4ade80"),
        fontSize: 10,
        textTransform: "uppercase",
      }}>{port.kind}</span>
      <span
        title={port.alsa_name}
        style={{
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
          flex: 1,
          textAlign: isSource ? "right" : "left",
        }}
      >
        {port.label}
      </span>
      <Handle
        type={side}
        position={isSource ? Position.Right : Position.Left}
        id={port.id}
        style={{
          background: isSource ? "#6cf" : "#4ade80",
          width: 10,
          height: 10,
          border: "2px solid #0b0d10",
        }}
      />
    </div>
  );
}
