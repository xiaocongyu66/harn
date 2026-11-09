import path from "node:path";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// dev 时把 /ws 代理到本地 harn --web（默认端口）；嵌入构建不依赖此配置。
const HARN_WEB_PORT = process.env.HARN_WEB_PORT ?? "7331";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  build: {
    outDir: "dist",
    sourcemap: false,
    target: "es2022",
    chunkSizeWarningLimit: 1500,
  },
  server: {
    proxy: { "/ws": `http://127.0.0.1:${HARN_WEB_PORT}` },
  },
});
