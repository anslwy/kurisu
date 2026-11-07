import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Dev: `npm run dev` serves the UI on :5173 and proxies /api to a running
// `ku web` (default 127.0.0.1:8787; override with KURISU_DEV_API).
const api = process.env.KURISU_DEV_API || "http://127.0.0.1:8787";

export default defineConfig({
  plugins: [svelte()],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true, assetsDir: "assets", sourcemap: false, target: "es2022" },
  server: { proxy: { "/api": { target: api, changeOrigin: true } } },
});
