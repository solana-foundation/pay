import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  base: "./",
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/__debugger": "http://localhost:3000",
      "/mpp": "http://localhost:3000",
      "/x402": "http://localhost:3000",
      "/facilitator": "http://localhost:3000",
      "/health": "http://localhost:3000",
    },
  },
});
