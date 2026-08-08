import { create } from "zustand";
import type {
  ClientMsg,
  LocalPort,
  Node,
  NodeId,
  Route,
  RouteId,
  ServerMsg,
  StateSnapshot,
  StreamStats,
} from "./protocol";

interface Store {
  connected: boolean;
  self?: Node;
  nodes: Record<NodeId, Node>;
  ports: Record<NodeId, LocalPort[]>;
  routes: Record<RouteId, Route>;
  stats: Record<RouteId, StreamStats>;
  send: (msg: ClientMsg) => void;
  _socket?: WebSocket;
  _connect: () => void;
}

export const useStore = create<Store>((set, get) => ({
  connected: false,
  nodes: {},
  ports: {},
  routes: {},
  stats: {},
  send: (msg) => {
    const ws = get()._socket;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg));
    }
  },
  _connect: () => {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const url = `${scheme}://${location.host}/ws`;
    const ws = new WebSocket(url);

    ws.onopen = () => set({ connected: true, _socket: ws });
    ws.onclose = () => {
      set({ connected: false });
      // Auto-reconnect.
      setTimeout(() => get()._connect(), 1500);
    };
    ws.onerror = () => ws.close();
    ws.onmessage = (evt) => {
      let msg: ServerMsg;
      try {
        msg = JSON.parse(evt.data) as ServerMsg;
      } catch {
        return;
      }
      applyServerMsg(msg, set, get);
    };
    set({ _socket: ws });
  },
}));

function applyServerMsg(
  msg: ServerMsg,
  set: (partial: Partial<Store>) => void,
  get: () => Store,
): void {
  switch (msg.type) {
    case "state": {
      const snap: StateSnapshot = msg.snapshot;
      const nodes: Record<NodeId, Node> = {};
      const ports: Record<NodeId, LocalPort[]> = {};
      const routes: Record<RouteId, Route> = {};

      nodes[snap.self_node.id] = snap.self_node;
      ports[snap.self_node.id] = snap.local_ports;
      for (const n of snap.nodes) nodes[n.id] = n;
      for (const p of snap.remote_ports) {
        (ports[p.node_id] ??= []).push(p);
      }
      for (const r of snap.routes) routes[r.id] = r;

      set({ self: snap.self_node, nodes, ports, routes });
      break;
    }
    case "node_appeared":
      set({
        nodes: { ...get().nodes, [msg.node.id]: msg.node },
        ports: { ...get().ports, [msg.node.id]: msg.ports },
      });
      break;
    case "node_disappeared": {
      const { [msg.node_id]: _n, ...restNodes } = get().nodes;
      const { [msg.node_id]: _p, ...restPorts } = get().ports;
      set({ nodes: restNodes, ports: restPorts });
      break;
    }
    case "route_added":
    case "route_updated":
      set({ routes: { ...get().routes, [msg.route.id]: msg.route } });
      break;
    case "route_removed": {
      const { [msg.id]: _r, ...restRoutes } = get().routes;
      set({ routes: restRoutes });
      break;
    }
    case "stats":
      set({ stats: msg.stats });
      break;
    case "error":
      console.warn("engine error:", msg.message);
      break;
  }
}
