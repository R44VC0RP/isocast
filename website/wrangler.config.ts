import { defineWranglerConfig } from "wrangler/experimental-config";

// The site is the static files in public/, served as they are. This folder's
// config, node_modules and build output (.cloudflare/) are never uploaded.
export default defineWranglerConfig({
  assetsDirectory: "public",
});
