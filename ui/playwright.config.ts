import { defineConfig } from "@playwright/test";

// Suites write their evidence (screenshots, summary.json) under e2e/artifacts/<suite>/.
export default defineConfig({
  testDir: "./e2e",
  outputDir: "./test-results",
  reporter: [["list"], ["html", { outputFolder: "./playwright-report", open: "never" }]],
  use: {
    launchOptions: { executablePath: "/usr/bin/google-chrome" },
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "gallery",
      testMatch: /gallery\.spec\.ts/,
      use: { baseURL: "http://127.0.0.1:5173", viewport: { width: 1440, height: 900 } },
    },
    {
      name: "shell",
      testMatch: /shell\.spec\.ts/,
      use: { baseURL: "http://127.0.0.1:5173", viewport: { width: 1440, height: 900 } },
    },
    {
      // Live suites: each worker runs a real `arena0 ui` (see e2e/harness.ts).
      name: "app",
      testMatch: /e2e\/app\/[^/]+\.spec\.ts/,
      timeout: 120_000,
    },
  ],
  webServer: {
    command: "pnpm vite --port 5173 --strictPort",
    url: "http://127.0.0.1:5173/gallery",
    reuseExistingServer: true,
  },
});
