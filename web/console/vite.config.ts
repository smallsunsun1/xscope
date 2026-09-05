import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const configProcess = (globalThis as typeof globalThis & {
  process?: { cwd?: () => string; env?: { XSCOPE_BAZEL_BUILD?: string } };
}).process;
const bazelBuild = configProcess?.env?.XSCOPE_BAZEL_BUILD === "1";
const consoleRoot = new URL(".", import.meta.url).pathname;
const outputDirectory = bazelBuild ? `${configProcess?.cwd?.() ?? "."}/dist` : "dist";

export default defineConfig({
  root: consoleRoot,
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8081",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/api/, ""),
      },
      "/gateway": {
        target: "http://127.0.0.1:8080",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/gateway/, ""),
      },
      "/runtime": {
        target: "http://127.0.0.1:8090",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/runtime/, ""),
      },
      "/operator": {
        target: "http://127.0.0.1:8082",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/operator/, ""),
      },
    },
  },
  build: {
    outDir: outputDirectory,
    emptyOutDir: !bazelBuild,
    sourcemap: true,
  },
});
