import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The Rust server embeds dist/ and serves it same-origin, so /api is relative.
// In dev, Vite proxies /api to a running server (OP_SERVER, default 127.0.0.1:7878).
const target = process.env.OP_SERVER ?? "http://127.0.0.1:7878";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: { "/api": { target, ws: true } },
  },
  build: { outDir: "dist", chunkSizeWarningLimit: 1500 },
});
