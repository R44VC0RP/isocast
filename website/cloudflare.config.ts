import { defineConfig } from "cf/config";

// isocast.com: a Worker with static assets (public/) and no code of its own,
// on the work account, zone isocast.com. Deploy: docs/website.md.
export default defineConfig({
  worker: {
    name: "isocast-site",
    compatibilityDate: "2026-10-19",
    assets: { notFoundHandling: "404-page", htmlHandling: "auto-trailing-slash" },
    domains: ["isocast.com", "www.isocast.com"],
    observability: { enabled: true },
  },
});
