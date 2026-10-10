import { defineConfig } from "/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web/node_modules/vite/dist/node/index.js";
export default defineConfig({
  root: new URL("./vite-root", import.meta.url).pathname,
  server: {
    host: "127.0.0.1",
    port: 8097,
    strictPort: true,
    proxy: { "/ws": { target: "http://127.0.0.1:8099", ws: true, changeOrigin: true } },
  },
});
