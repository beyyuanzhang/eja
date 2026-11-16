import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

const appPort = 4173;
const apiProxy = {
  "/api": "http://127.0.0.1:8787",
};

export default defineConfig({
  plugins: [svelte()],
  base: "./",
  server: {
    host: "127.0.0.1",
    port: appPort,
    strictPort: true,
    proxy: apiProxy,
  },
  preview: {
    host: "127.0.0.1",
    port: appPort,
    strictPort: true,
    proxy: apiProxy,
  },
  build: {
    outDir: "../public",
    emptyOutDir: true,
    sourcemap: false,
  },
});
