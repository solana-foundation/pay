import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";

// Second app: the pay-cloud onboarding page. Built independently of the
// debugger (`vite.config.ts` → dist/) into dist-cloud/, which
// rust/crates/cloud embeds with include_dir.
export default defineConfig({
  root: "cloud",
  base: "/",
  publicDir: false,
  plugins: [react()],
  resolve: {
    // The cloud HTML lives one directory below the shared React source. In
    // dev, its `../src` entry is normalized to `/src`, so map that URL back to
    // the actual source directory instead of looking under `cloud/src`.
    alias: {
      "/src": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    outDir: "../dist-cloud",
    emptyOutDir: true,
  },
  server: {
    port: 5174,
    proxy: {
      "/api": "http://127.0.0.1:8402",
      "/v1": "http://127.0.0.1:8402",
    },
  },
});
