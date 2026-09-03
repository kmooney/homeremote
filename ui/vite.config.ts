import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  build: {
    // The bundle is embedded in the Rust binary, so keep it to a handful of
    // files rather than many small chunks.
    chunkSizeWarningLimit: 900,
  },
  server: {
    // `npm run dev` proxies to a daemon started with `homeremote serve`.
    proxy: {
      "/api": {
        target: "http://localhost:8385",
        ws: true,
      },
    },
  },
});
