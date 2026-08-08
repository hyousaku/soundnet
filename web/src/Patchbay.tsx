import { useMemo, useCallback } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  type Edge,
  type OnConnect,
  MarkerType,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { useStore } from "./store";
import PortNode, { type PortRFNode } from "./PortNode";
import { defaultSpec, type PortRef, type Route } from "./protocol";

const nodeTypes = { port: PortNode };

export default function Patchbay() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const routes = useStore((s) => s.routes);
  const stats = useStore((s) => s.stats);
  const send = useStore((s) => s.send);

  const rfNodes: PortRFNode[] = useMemo(() => {
    return Object.values(nodes).map((n, i) => ({
      id: n.id,
      type: "port" as const,
      position: { x: 40 + i * 360, y: 40 },
      data: {
        node: n,
        ports: ports[n.id] ?? [],
      },
    }));
  }, [nodes, ports]);

  const rfEdges: Edge[] = useMemo(() => {
    return Object.values(routes).map((r) => {
      const s = stats[r.id];
      const lat = s ? ` · e2e ${s.e2e_latency_ms.toFixed(1)}ms` : "";
      const xr = s && s.xruns > 0 ? ` · xr ${s.xruns}` : "";
      return {
        id: r.id,
        source: r.src.node_id,
        sourceHandle: r.src.port_id,
        target: r.dst.node_id,
        targetHandle: r.dst.port_id,
        label: `${r.spec.rate / 1000}k · ${r.spec.channels}ch · ${r.spec.frames_per_period}f${lat}${xr}`,
        labelBgStyle: { fill: "#151920" },
        labelStyle: { fill: "#e6e9ef", fontSize: 11 },
        style: { stroke: "#6cf", strokeWidth: 2 },
        markerEnd: { type: MarkerType.ArrowClosed, color: "#6cf" },
      };
    });
  }, [routes, stats]);

  const onConnect: OnConnect = useCallback(
    (params) => {
      if (!params.source || !params.target || !params.sourceHandle || !params.targetHandle) {
        return;
      }
      const route: Route = {
        id: "",
        src: {
          node_id: params.source,
          port_id: params.sourceHandle,
          channel_offset: 0,
        } as PortRef,
        dst: {
          node_id: params.target,
          port_id: params.targetHandle,
          channel_offset: 0,
        } as PortRef,
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
        nodeTypes={nodeTypes}
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
