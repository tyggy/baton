import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

// Tests run inside workerd (Miniflare) with the real Durable Object. The
// secrets below are test-only placeholders; APNS_KEY is replaced per test
// with a freshly generated P-256 key, and APNs itself is mocked (no network).
export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          APNS_KEY: "",
          APNS_KEY_ID: "TESTKEYID1",
          APNS_TEAM_ID: "TESTTEAM01",
          APNS_TOPIC: "dev.baton.test",
          REGISTRATION_SECRET: "test-registration-secret-0123456789abcdef",
        },
      },
    }),
  ],
});
