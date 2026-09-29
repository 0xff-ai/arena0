import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// `pnpm dev` serves the UI with HMR and proxies the WebSocket to a gateway
// started with `arena0 ui --port 7357 --dev-origin http://127.0.0.1:5173`.
// The live e2e harness runs one server per worker and points it at that
// worker's gateway through ARENA0_GATEWAY (host:port).
const gateway = process.env.ARENA0_GATEWAY ?? "127.0.0.1:7357";
export default defineConfig({
  plugins: [react({ compiler: true }), tailwindcss()],
  resolve: { alias: { "~": new URL("./src", import.meta.url).pathname } },
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: { "/ws": { target: `ws://${gateway}`, ws: true } },
  },
  build: { target: "es2023", sourcemap: true },
});
