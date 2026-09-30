import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Each live browser worker points these same-origin routes at its own daemon.
const daemon = process.env.ARENA0_DAEMON_URL ?? "http://127.0.0.1:7357";
export default defineConfig({
  plugins: [react({ compiler: true }), tailwindcss()],
  resolve: { alias: { "~": new URL("./src", import.meta.url).pathname } },
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: {
      "/rpc": { target: daemon },
      "/events": {
        target: daemon,
        // Vite pipes the daemon's stream to the page but leaves the page's end
        // open when the daemon's end drops mid-stream. Drop it too, so the page
        // sees a dead daemon as it does against the embedded UI.
        configure: (proxy) =>
          proxy.on("proxyRes", (proxyRes, _req, res) =>
            proxyRes.on("close", () => {
              if (!proxyRes.complete) res.destroy();
            }),
          ),
      },
      "/uploads": { target: daemon },
      "^/hosts/[^/]+/blobs/": { target: daemon },
    },
  },
  build: { target: "es2023", sourcemap: true },
});
