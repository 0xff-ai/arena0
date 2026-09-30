// Shell suite: what the workspace shell shows without a daemon. The live
// workspace is proven by the app suite against the real `arena0 ui`; there is
// no fake daemon here. Evidence goes to e2e/artifacts/shell/: a screenshot per
// state and theme, and summary.json listing each check that passed. Run with
// `pnpm playwright test --project shell`.
import * as fs from "node:fs";
import { expect, test } from "@playwright/test";

const THEMES = ["light", "dark"] as const;

test.use({ locale: "en-US", timezoneId: "UTC", reducedMotion: "reduce" });

const passed: string[] = [];

async function check(name: string, body: () => Promise<void>): Promise<void> {
  await test.step(name, body);
  passed.push(name);
}

for (const theme of THEMES) {
  test.describe(`${theme} theme`, () => {
    test.use({ colorScheme: theme });

    test(`without a daemon: Connecting, no workspace (${theme})`, async ({ page }, testInfo) => {
      const shots = `${testInfo.project.testDir}/artifacts/shell/${theme}`;
      fs.mkdirSync(shots, { recursive: true });
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));

      await page.goto("/");
      await check(`${theme}: no daemon shows Connecting to arena0d…`, async () => {
        await expect(page.getByText("Connecting to arena0d…")).toBeVisible();
      });
      await check(`${theme}: the workspace is not rendered before the first ready`, async () => {
        await expect(page.getByRole("button", { name: "Find" })).toHaveCount(0);
        await expect(page.getByRole("tab", { name: /Sessions/ })).toHaveCount(0);
        await expect(page.getByText("New session")).toHaveCount(0);
      });
      await page.screenshot({ path: `${shots}/connecting.png` });
      await check(`${theme}: connecting raises no page errors`, async () => {
        expect(errors).toEqual([]);
      });
    });
  });
}

test.afterAll(async ({}, testInfo) => {
  const dir = `${testInfo.project.testDir}/artifacts/shell`;
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    `${dir}/summary.json`,
    `${JSON.stringify({ suite: "shell", passed }, null, 2)}\n`,
  );
});
