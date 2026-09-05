import { defineConfig, devices } from "@playwright/test";
import path from "node:path";

const artifacts = process.env.TEST_UNDECLARED_OUTPUTS_DIR ?? path.resolve("test-results");
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: true,
  retries: 0,
  workers: 2,
  timeout: 30_000,
  outputDir: path.join(artifacts, "artifacts"),
  reporter: [["list"], ["html", { outputFolder: path.join(artifacts, "report"), open: "never" }]],
  use: {
    baseURL: "http://127.0.0.1:4187",
    locale: "en-US",
    timezoneId: "UTC",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    serviceWorkers: "block",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 1000 } } },
    { name: "mobile-chromium", use: { ...devices["Pixel 7"] }, testMatch: /mobile\.spec\.ts/ },
  ],
  webServer: {
    command: `"${process.execPath}" web/console/e2e/server.mjs`,
    cwd: process.cwd(),
    url: "http://127.0.0.1:4187",
    reuseExistingServer: false,
    timeout: 20_000,
  },
});
