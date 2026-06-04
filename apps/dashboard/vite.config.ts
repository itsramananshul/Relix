import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The bridge serves the built bundle at /dashboard, so every asset URL
// must be prefixed accordingly. Output goes straight into the web-bridge
// crate's `dashboard-dist` so `npm run build` is the whole pipeline:
// the bridge's `dashboard::resolve_spa_dir()` picks it up from there.
//
// `modulePreload.polyfill = false` keeps Vite from injecting an inline
// bootstrap <script>, so the page loads under the bridge's strict default
// CSP (`script-src 'self'`, no `'unsafe-inline'`).
export default defineConfig({
  plugins: [react()],
  base: "/dashboard/",
  build: {
    outDir: "../../crates/relix-web-bridge/dashboard-dist",
    emptyOutDir: true,
    modulePreload: { polyfill: false },
    target: "es2020",
  },
  server: {
    port: 5273,
    // During `npm run dev`, proxy API + auth to a locally running bridge.
    proxy: {
      "/v1": { target: "http://127.0.0.1:19791", changeOrigin: false },
      "/health": { target: "http://127.0.0.1:19791", changeOrigin: false },
    },
  },
});
