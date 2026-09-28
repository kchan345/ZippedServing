import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests-web",
  testMatch: "**/*.spec.js",
  workers: 1,
  timeout: 60_000,
  use: {
    channel: "msedge",
    headless: true,
    trace: "retain-on-failure",
    screenshot: "only-on-failure"
  }
});
