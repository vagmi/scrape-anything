import type { Config } from "@react-router/dev/config";

export default {
  // SPA mode: no server rendering. Build emits static assets in build/client,
  // served by the Rust backend with an index.html fallback for client routes.
  ssr: false,
} satisfies Config;
