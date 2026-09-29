// Live workspace shell: the workspace against a real `arena0 ui`. Evidence goes to
// e2e/artifacts/app-workspace/.
import { Evidence, expect, openApp, seed, test, watchConsole } from "../harness";

const evidence = new Evidence("app-workspace");
test.use({ suite: "app-workspace" });
test.afterAll(() => evidence.write());

test("the shell syncs Hosts, programs and a completed session", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await seed.completed(arena, "rock-paper-scissors");
  await openApp(page, arena, "/sessions");

  await evidence.check("the Sessions tab counts the completed session", async () => {
    await expect(page.getByRole("tab", { name: /Sessions\s*1/ })).toBeVisible();
  });
  await evidence.check("the Programs tab counts the bundled programs", async () => {
    await expect(page.getByRole("tab", { name: /Programs\s*[1-9]/ })).toBeVisible();
  });
  await evidence.check("the Receipts tab counts one receipt per Host", async () => {
    await expect(page.getByRole("tab", { name: /Receipts\s*2/ })).toBeVisible();
  });
  await evidence.shots(page, "sessions");
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("reconnecting reloads missed sessions and preserves observed activity", async ({
  page,
  arena,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await openApp(page, arena, "/sessions");
  await seed.completed(arena, "rock-paper-scissors");
  const listing: { executions: unknown[] } = JSON.parse(
    await arena.cli(["--json", "--host", "host-01", "exec", "list"]),
  );
  const count = listing.executions.length;
  await expect(page.getByRole("tab", { name: new RegExp(`Sessions\\s*${count}$`) })).toBeVisible();
  const signals = page.getByRole("region", { name: "Signals" });
  await signals.getByRole("tab", { name: /^Activity/ }).click();
  const activity = signals.getByRole("grid", { name: "Activity" });
  // Activity is virtualized newest-first, so retain an observed terminal row
  // that is visible without scrolling back to the execution's creation.
  const completed = activity.getByRole("row").filter({ hasText: "session completed" }).first();
  await expect(completed).toBeVisible();
  const observed = (await completed.textContent())!;

  await evidence.check("a dropped connection keeps the workspace visible", async () => {
    await page.context().setOffline(true);
    await expect(page.getByTestId("connection")).toHaveText(/offline|connecting/);
    await expect(page.getByRole("button", { name: "Find", exact: true })).toBeVisible();
  });
  // The daemon keeps running while the browser is disconnected.
  await seed.completed(arena, "rock-paper-scissors");
  await page.context().setOffline(false);
  await evidence.check(
    "reconnect resets rows to include work completed while offline",
    async () => {
      await expect(page.getByTestId("connection")).toHaveText(/live/, { timeout: 30_000 });
      await expect(
        page.getByRole("tab", { name: new RegExp(`Sessions\\s*${count + 1}$`) }),
      ).toBeVisible();
      await expect(activity).toContainText(observed);
      expect(errors).toEqual([]);
    },
  );
  await evidence.shots(page, "reconnected");
});
