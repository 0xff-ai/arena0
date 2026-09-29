// Shell suite: what the workspace shell shows without a gateway. The live
// workspace is proven by the M4 suite against the real `arena0 ui`; there is
// no fake gateway here. Evidence goes to e2e/artifacts/shell/: a screenshot per
// state and theme, and summary.json listing each check that passed. Run with
// `pnpm playwright test --project shell`.
import { expect, test } from "@playwright/test";

const THEMES = ["light", "dark"] as const;

test.use({ locale: "en-US", timezoneId: "UTC", reducedMotion: "reduce" });

interface Fs {
  mkdirSync(path: string, options: { recursive: true }): void;
  writeFileSync(path: string, data: string): void;
}
// The project ships no Node type declarations, so `node:fs` is loaded through a
// computed specifier and given the two-function shape used here.
const FS_MODULE: string = "node:fs";
const loadFs = async () => (await import(FS_MODULE)) as Fs;

const passed: string[] = [];

async function check(name: string, body: () => Promise<void>): Promise<void> {
  await test.step(name, body);
  passed.push(name);
}

for (const theme of THEMES) {
  test.describe(`${theme} theme`, () => {
    test.use({ colorScheme: theme });

    test(`no token: the page says how to get a link (${theme})`, async ({ page }, testInfo) => {
      const shots = `${testInfo.project.testDir}/artifacts/shell/${theme}`;
      (await loadFs()).mkdirSync(shots, { recursive: true });
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));

      await page.goto("/");
      await check(`${theme}: no token shows the EmptyState and the command`, async () => {
        await expect(page.getByText("Open the link printed by `arena0 ui`")).toBeVisible();
        await expect(page.locator("code", { hasText: "arena0 ui" })).toBeVisible();
      });
      await check(`${theme}: no token renders no workspace`, async () => {
        await expect(page.getByRole("button", { name: "Find" })).toHaveCount(0);
        await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      });
      await page.screenshot({ path: `${shots}/no-token.png` });
      await check(`${theme}: no token raises no page errors`, async () => {
        expect(errors).toEqual([]);
      });
    });

    test(`token without a gateway: Connecting, no workspace (${theme})`, async ({
      page,
    }, testInfo) => {
      const shots = `${testInfo.project.testDir}/artifacts/shell/${theme}`;
      (await loadFs()).mkdirSync(shots, { recursive: true });
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));

      await page.goto("/#token=00");
      await check(`${theme}: a token with no gateway shows Connecting to arena0d…`, async () => {
        await expect(page.getByText("Connecting to arena0d…")).toBeVisible();
      });
      await check(`${theme}: the workspace is not rendered before the first ready`, async () => {
        await expect(page.getByRole("button", { name: "Find" })).toHaveCount(0);
        await expect(page.getByRole("tab", { name: /Sessions/ })).toHaveCount(0);
        await expect(page.getByText("New session")).toHaveCount(0);
      });
      await check(`${theme}: the token is stripped from the address bar`, async () => {
        await expect(page).toHaveURL(/^http:\/\/127\.0\.0\.1:5173\/$/);
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
  const fs = await loadFs();
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    `${dir}/summary.json`,
    `${JSON.stringify({ suite: "shell", passed }, null, 2)}\n`,
  );
});
