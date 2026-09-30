// Live list documents: sessions, offers, receipts and programs against a real
// `arena0 ui`. Evidence goes to e2e/artifacts/app-lists/.
import { Evidence, expect, openApp, openList, seed, test, watchConsole } from "../harness";

const evidence = new Evidence("app-lists");
test.use({ suite: "app-lists" });
test.afterAll(() => evidence.write());

test("the list documents show what the daemon holds", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await seed.completed(arena, "rock-paper-scissors");
  await seed.completed(arena, "prisoner-dilemma");
  await openApp(page, arena, "/sessions");

  const sessions = page.getByRole("grid", { name: "Sessions" });
  const dataRows = (grid: typeof sessions) =>
    grid.getByRole("row").filter({ has: page.getByRole("gridcell") });
  const statusBar = page.getByRole("contentinfo");

  await evidence.check("with only finished sessions, Live offers to show them all", async () => {
    await expect(page.getByText("No live sessions")).toBeVisible();
    await page.getByRole("button", { name: "Show all 2 sessions" }).click();
    await expect(dataRows(sessions)).toHaveCount(2);
    await expect(page).toHaveURL(/[?&]state=all/);
  });

  await seed.awaitingYou(arena, "chess");
  await openApp(page, arena, "/sessions");

  await evidence.check("Live shows the active chess session and no completed one", async () => {
    await expect(dataRows(sessions)).toHaveCount(1);
    await expect(sessions.getByRole("row", { name: /chess/i })).toBeVisible();
    await expect(sessions.getByRole("img", { name: "active" })).toBeVisible();
    await expect(sessions.getByRole("row", { name: /rock-paper-scissors|prisoner/i })).toHaveCount(
      0,
    );
    await expect(page.getByText("1 session", { exact: true })).toBeVisible();
  });
  await evidence.shots(page, "sessions-live");

  await evidence.check("All shows all three sessions and the count matches", async () => {
    await page.getByRole("radio", { name: /^All/ }).click();
    await expect(dataRows(sessions)).toHaveCount(3);
    await expect(page.getByText("3 sessions", { exact: true })).toBeVisible();
  });
  await evidence.shots(page, "sessions-all");

  await evidence.check("Find narrows the list and lives in the URL", async () => {
    await page.getByRole("searchbox", { name: "Find" }).fill("rock");
    await expect(dataRows(sessions)).toHaveCount(1);
    await expect(sessions.getByRole("row", { name: /rock-paper-scissors/i })).toBeVisible();
    await expect(page).toHaveURL(/[?&]q=rock/);
    await page.getByRole("searchbox", { name: "Find" }).fill("");
    await expect(dataRows(sessions)).toHaveCount(3);
  });

  await evidence.check("By host shows one section per Host", async () => {
    await page.getByRole("radio", { name: "By host" }).click();
    for (const host of ["host-01", "host-02"]) {
      await expect(page.getByRole("grid", { name: `Sessions on ${host}` })).toBeVisible();
    }
    await expect(page.getByRole("grid", { name: /^Sessions on / })).toHaveCount(2);
  });
  await evidence.shots(page, "sessions-by-host");
  await page.getByRole("radio", { name: "List" }).click();

  await evidence.check("Click selects, Enter previews, double-click on the tab pins", async () => {
    const chess = sessions.getByRole("row", { name: /chess/i });
    await chess.click();
    await expect(statusBar).toContainText(/chess [0-9a-f]{8} · \w+/i);
    await chess.press("Enter");
    const tab = page.getByRole("tab", { name: /chess/i });
    await expect(tab).toHaveAttribute("data-preview", "true");
    await tab.dblclick();
    await expect(tab).not.toHaveAttribute("data-preview", "true");
    await page.getByRole("tab", { name: /^Sessions/ }).click();
    await expect(sessions).toBeVisible();
  });

  await evidence.check("narrow lists drop Evidence and Age", async () => {
    await page.setViewportSize({ width: 2400, height: 900 });
    await expect(page.getByRole("columnheader", { name: "Evidence" })).toBeVisible();
    await expect(page.getByRole("columnheader", { name: "Age" })).toBeVisible();
    await page.setViewportSize({ width: 900, height: 900 });
    await expect(page.getByRole("columnheader", { name: "Evidence" })).toHaveCount(0);
    await expect(page.getByRole("columnheader", { name: "Age" })).toHaveCount(0);
  });
  await evidence.shots(page, "sessions-narrow");
  await page.setViewportSize({ width: 1440, height: 900 });

  await evidence.check(
    "Receipts lists one per Host per completed session, filters, opens",
    async () => {
      await openList(page, "Receipts");
      const receipts = page.getByRole("grid", { name: "Receipts" });
      // The chess session may end with stop reports of its own, so count the completed sessions'.
      const of = (program: RegExp) => dataRows(receipts).filter({ hasText: program });
      await expect(of(/Rock-Paper-Scissors/)).toHaveCount(2);
      await expect(of(/Prisoner's Dilemma/)).toHaveCount(2);
      await evidence.shots(page, "receipts");
      await page.getByRole("button", { name: /Filter by Host/ }).click();
      await page.getByRole("option", { name: "host-01" }).click();
      await expect(page).toHaveURL(/[?&]host=host-01/);
      await expect(of(/Rock-Paper-Scissors/)).toHaveCount(1);
      await expect(of(/Prisoner's Dilemma/)).toHaveCount(1);
      await expect(dataRows(receipts).filter({ hasText: "host-02" })).toHaveCount(0);
      await of(/Rock-Paper-Scissors/).press("Enter");
      await expect(page).toHaveURL(/\/receipts\/host-01\/[^/]+$/);
    },
  );

  await evidence.check("Programs lists the bundled programs and starts a session", async () => {
    await openList(page, "Programs");
    const programs = page.getByRole("grid", { name: "Programs" });
    for (const name of [
      "chess",
      "vickrey-auction",
      "prisoner-dilemma",
      "rock-paper-scissors",
      "cumulative-sum",
      "sequential-count",
      "contract-net",
    ]) {
      await expect(programs.getByRole("row", { name: new RegExp(name, "i") })).toBeVisible();
    }
    const chess = programs.getByRole("row", { name: /chess/i });
    await expect(chess).toContainText("host-01, host-02");
    // Whether the chess session still lives is a race with its own abort, so only a program that never ran has a fixed live count.
    await expect(
      programs.getByRole("row", { name: /vickrey-auction/i }).getByRole("gridcell", { name: "—" }),
    ).toBeVisible();
    await evidence.shots(page, "programs");
    await page.getByRole("button", { name: "New chess session" }).click();
    await expect(page).toHaveURL(/\/programs\/[0-9a-f]+\?launch=true$/);
    await openList(page, "Programs");
  });

  await evidence.check("Remove asks first, then the program is gone", async () => {
    const programs = page.getByRole("grid", { name: "Programs" });
    const before = await dataRows(programs).count();
    await page.getByRole("button", { name: "Remove sequential-count" }).click();
    const dialog = page.getByRole("alertdialog");
    await expect(dialog).toContainText("sequential-count");
    await expect(dialog).toContainText("host-01, host-02");
    await evidence.shots(page, "programs-remove-confirm");
    await dialog.getByRole("button", { name: "Remove" }).click();
    await expect(programs.getByRole("row", { name: /sequential-count/i })).toHaveCount(0);
    await expect(dataRows(programs)).toHaveCount(before - 1);
  });

  await evidence.check("Offers shows the empty state", async () => {
    await openList(page, "Offers");
    await expect(page.getByText("No offers seen")).toBeVisible();
  });
  await evidence.shots(page, "offers-empty");

  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});
