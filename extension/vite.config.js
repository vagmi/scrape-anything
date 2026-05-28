import { defineConfig } from "vite";
import { crx } from "@crxjs/vite-plugin";
import tailwindcss from "@tailwindcss/vite";
import manifest from "./manifest.json" with { type: "json" };

export default defineConfig({
  plugins: [tailwindcss(), crx({ manifest })],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    // crxjs uses a port for HMR; keep stable so reloads aren't disruptive.
    port: 5173,
    strictPort: true,
  },
});
