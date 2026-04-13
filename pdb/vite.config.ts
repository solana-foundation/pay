import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  base: "/__402/pdb/",
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/__402": "http://localhost:3000",
      "/mpp": "http://localhost:3000",
      "/x402": "http://localhost:3000",
      "/facilitator": "http://localhost:3000",
      "/health": "http://localhost:3000",
    },
  },
});
