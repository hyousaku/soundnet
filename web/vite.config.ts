import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// In dev, the Vite dev server proxies REST and WS traffic to the local engine
// so we can hot-reload the UI while the daemon runs on the standard port.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:7788",
      "/ws": { target: "ws://127.0.0.1:7788", ws: true },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
  },
});
