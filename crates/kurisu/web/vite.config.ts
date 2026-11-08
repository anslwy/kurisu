import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Two pages share this build:
//   index.html → the `ku web` PWA (served by web.rs)
//   chat.html  → the /web chat app behind a Telegram one-time link (chat.rs)
// `base: "./"` matters: chat.html is served under /_web/, so its assets must
// resolve relative to that path, and everything is embedded in the binary.
//
// Dev: `npm run dev` serves the UI on :5173 and proxies /api to a running
// `ku web` (default 127.0.0.1:8787; override with KURISU_DEV_API).
const api = process.env.KURISU_DEV_API || "http://127.0.0.1:8787";

export default defineConfig({
  plugins: [svelte()],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    sourcemap: false,
    target: "es2022",
    rollupOptions: { input: { main: "index.html", chat: "chat.html" } },
  },
  server: { proxy: { "/api": { target: api, changeOrigin: true } } },
});
