import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// ADR-015. Dev: Vite at :5173 proxies /api/* and /chat/ws to the axum
// server at :8092. Prod: axum serves `dist/` directly (no Vite at all).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": "http://localhost:8092",
      "/chat/ws": { target: "ws://localhost:8092", ws: true },
      // Image-generation surface (ADR-019): API + served PNGs proxy to axum;
      // the SPA route /gallery itself stays with Vite for HMR.
      "/gallery/api": "http://localhost:8092",
      "/gallery/files": "http://localhost:8092",
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
