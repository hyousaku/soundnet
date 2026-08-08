import { useMemo, useCallback } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  type Node as RFNode,
  type Edge,
  type OnConnect,
  MarkerType,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { useStore } from "./store";
import { defaultSpec, type PortRef, type Route } from "./protocol";

// Turn (node, ports, routes) into React-Flow nodes/edges.
export default function Patchbay() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const routes = useStore((s) => s.routes);
  const send = useStore((s) => s.send);

  const rfNodes: RFNode[] = useMemo(() => {
    const out: RFNode[] = [];
    let col = 0;
    for (const n of Object.values(nodes)) {
      const nodePorts = ports[n.id] ?? [];
      const captureCount = nodePorts.filter((p) => p.kind !== "playback").length;
      const playbackCount = nodePorts.filter((p) => p.kind === "playback").length;
      const height = 60 + Math.max(captureCount, playbackCount) * 22;
      out.push({
        id: n.id,
        position: { x: 40 + col * 340, y: 80 },
        type: "default",
        data: {
          label: (
            <NodeCard node={n} ports={nodePorts} />
          ) as unknown as string,
        },
        style: {
          background: "#1c222b",
          color: "#e6e9ef",
          border: "1px solid #262d38",
          borderRadius: 8,
          width: 300,
          height,
          padding: 0,
        },
      });
      col++;
    }
    return out;
  }, [nodes, ports]);

  const rfEdges: Edge[] = useMemo(() => {
    return Object.values(routes).map((r) => ({
      id: r.id,
      source: r.src.node_id,
      target: r.dst.node_id,
      label: `${r.spec.rate / 1000}k · ${r.spec.channels}ch · ${r.spec.frames_per_period}f · ${r.spec.target_latency_ms}ms`,
      labelBgStyle: { fill: "#151920" },
      labelStyle: { fill: "#e6e9ef", fontSize: 11 },
      style: { stroke: "#6cf", strokeWidth: 2 },
      markerEnd: { type: MarkerType.ArrowClosed, color: "#6cf" },
    }));
  }, [routes]);

  const onConnect: OnConnect = useCallback(
    (params) => {
      const src = params.source;
      const dst = params.target;
      if (!src || !dst) return;
      // Prompt for src/dst ports because React Flow default connect only carries
      // node ids. In a fuller UI we'd expose per-port handles; for the MVP we
      // pick the first Capture on src and first Playback on dst.
      const srcPort = (useStore.getState().ports[src] ?? []).find(
        (p) => p.kind !== "playback",
      );
      const dstPort = (useStore.getState().ports[dst] ?? []).find(
        (p) => p.kind === "playback",
      );
      if (!srcPort || !dstPort) {
        alert("No usable ports on one of the selected nodes.");
        return;
      }
      const route: Route = {
        id: "",
        src: { node_id: src, port_id: srcPort.id, channel_offset: 0 } as PortRef,
        dst: { node_id: dst, port_id: dstPort.id, channel_offset: 0 } as PortRef,
        spec: defaultSpec(),
      };
      send({ type: "add_route", route });
    },
    [send],
  );

  return (
    <div className="canvas">
      <ReactFlow
        nodes={rfNodes}
        edges={rfEdges}
        onConnect={onConnect}
        fitView
        proOptions={{ hideAttribution: true }}
      >
        <Background color="#262d38" gap={20} />
        <Controls />
        <MiniMap pannable style={{ background: "#0b0d10" }} />
      </ReactFlow>
    </div>
  );
}

function NodeCard({ node, ports }: { node: any; ports: any[] }) {
  return (
    <div style={{ padding: "8px 10px", textAlign: "left" }}>
      <div style={{ fontWeight: 600 }}>{node.hostname}</div>
      <div style={{ fontSize: 11, color: "#8a94a5" }}>{node.addr}:{node.port}</div>
      <div style={{ marginTop: 8, fontSize: 12 }}>
        {ports.map((p) => (
          <div key={p.id} style={{ display: "flex", justifyContent: "space-between" }}>
            <span style={{ color: p.kind === "playback" ? "#4ade80" : "#6cf" }}>
              {p.kind}
            </span>
            <span style={{ color: "#e6e9ef", marginLeft: 8, textAlign: "right" }}>
              {p.label}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
