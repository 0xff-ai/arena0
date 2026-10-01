import { defineConfig } from "@playwright/test";

// The gallery and shell suites run against a Vite server this run starts on
// ARENA0_UI_TEST_PORT (default 5173). It never reuses a server already on the
// port: that may be another checkout or project, and the suites would test it.
const port = Number(process.env.ARENA0_UI_TEST_PORT ?? 5173);
const origin = `http://127.0.0.1:${port}`;

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
      use: { baseURL: origin, viewport: { width: 1440, height: 900 } },
    },
    {
      name: "shell",
      testMatch: /shell\.spec\.ts/,
      use: { baseURL: origin, viewport: { width: 1440, height: 900 } },
    },
    {
      // Live suites: each worker runs a real `arena0 ui` (see e2e/harness.ts).
      name: "app",
      testMatch: /e2e\/app\/[^/]+\.spec\.ts/,
      timeout: 120_000,
    },
  ],
  webServer: {
    command: `pnpm vite --port ${port} --strictPort`,
    url: `${origin}/gallery`,
    reuseExistingServer: false,
  },
});
