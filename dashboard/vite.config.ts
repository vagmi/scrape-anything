import { reactRouter } from "@react-router/dev/vite";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [tailwindcss(), reactRouter()],
  resolve: {
    tsconfigPaths: true,
  },
  // Dev only: proxy API calls to the Rust backend so `npm run dev` on the SPA
  // hits the same endpoints it will use in production (same-origin at build).
  server: {
    proxy: {
      "/api": "http://localhost:4546",
    },
  },
});
