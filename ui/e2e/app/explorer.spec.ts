// Live Explorer, Signals and Timeline against a real `arena0 ui`. Evidence goes
// to e2e/artifacts/app-explorer/.
import { resolve } from "node:path";
import { Evidence, expect, openApp, seed, test, watchConsole } from "../harness";

const evidence = new Evidence("app-explorer");
test.use({ suite: "app-explorer" });
test.afterAll(() => evidence.write());

const SEQUENTIAL_COUNT = resolve(
  import.meta.dirname,
  "../../../programs/target/wasm32-unknown-unknown/release/sequential_count.wasm",
);

test("explorer, signals and timeline follow the live daemon", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await seed.completed(arena, "rock-paper-scissors");
  await seed.awaitingYou(arena, "chess");
  await openApp(page, arena, "/sessions");

  const explorer = page.getByRole("region", { name: "Explorer" });
  const signals = page.getByRole("region", { name: "Signals" });
  const timeline = page.getByRole("region", { name: "Timeline" });
  const hosts = explorer.getByRole("treegrid", { name: "Hosts" });
  const programs = explorer.getByRole("treegrid", { name: "Programs" });
  const receipts = explorer.getByRole("treegrid", { name: "Receipts" });
  const needs = signals.getByRole("grid", { name: "Needs input" });
  // host-01 plays a strategy, so its callouts come and go; host-02's stay open.
  const chessCallout = needs
    .getByRole("row")
    .filter({ hasText: "Chess" })
    .filter({ hasText: "host-02" });

  // The launch runs in the background: wait for its callout before counting live sessions.
  await expect(chessCallout).toHaveCount(1, {
    timeout: 30_000,
  });

  await evidence.check("Hosts lists host-01 and host-02 with their live counts", async () => {
    await expect(hosts.getByRole("row", { name: "host-01" })).toContainText("1 live");
    await expect(hosts.getByRole("row", { name: "host-02" })).toContainText("1 live");
  });
  await evidence.check("Programs lists chess with one live session", async () => {
    await expect(programs.getByRole("row", { name: "Chess" })).toContainText("1 live");
  });
  await evidence.check(
    "Receipts lists the rock-paper-scissors receipts, one per Host",
    async () => {
      await expect(receipts.getByRole("row", { name: /Receipt .* on host-01/ })).toHaveCount(1);
      await expect(receipts.getByRole("row", { name: /Receipt .* on host-02/ })).toHaveCount(1);
      await expect(receipts.getByRole("row", { name: /All receipts/ })).toBeVisible();
    },
  );
  await evidence.check(
    "the Explorer has Hosts, Programs and Receipts and no sessions",
    async () => {
      await expect(explorer.getByRole("treegrid")).toHaveCount(3);
      await expect(explorer.getByRole("heading", { level: 3 })).toHaveText([
        /Hosts/,
        /Programs/,
        /Receipts/,
      ]);
    },
  );
  await evidence.shots(page, "workspace-overview");

  await evidence.check("Enter on a Host opens its document tab", async () => {
    await hosts.getByRole("row", { name: "host-01" }).click();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/hosts\/host-01$/);
    await expect(page.getByRole("tab", { name: /host-01/ })).toBeVisible();
  });

  await evidence.check("Needs input lists the chess callout on host-02", async () => {
    await expect(chessCallout).toHaveCount(1);
  });
  await evidence.shots(page, "needs-input");
  await evidence.check("activating the callout opens the session and the composer", async () => {
    await chessCallout.click();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/s\//);
    await expect(page.getByRole("region", { name: "Composer" })).toBeVisible();
    await page.getByRole("button", { name: "Close composer" }).click();
    await expect(page.getByRole("region", { name: "Composer" })).toHaveCount(0);
  });

  await evidence.check("Problems shows its empty state with the minor switch", async () => {
    await signals.getByRole("tab", { name: /^Problems/ }).click();
    await expect(signals.getByText("No problems")).toBeVisible();
    await expect(signals.getByRole("switch", { name: /Include minor \(\d+\)/ })).toBeVisible();
  });

  await evidence.check("Activity lists rows newest first and filters warnings", async () => {
    await signals.getByRole("tab", { name: /^Activity/ }).click();
    const activity = signals.getByRole("grid", { name: "Activity" });
    const rows = activity.getByRole("row");
    await expect(rows.first()).toBeVisible();
    const clocks = await rows.evaluateAll((elements) =>
      elements.map((element) => element.textContent?.slice(0, 8) ?? ""),
    );
    expect(clocks.length).toBeGreaterThan(1);
    expect(clocks).toEqual([...clocks].sort().reverse());
    await expect(activity.getByRole("img", { name: "info" }).first()).toBeVisible();

    await signals.getByRole("radio", { name: "Warnings" }).click();
    await expect(activity.getByRole("img", { name: "info" })).toHaveCount(0);
    await signals.getByRole("radio", { name: "All" }).click();
    await expect(activity.getByRole("img", { name: "info" }).first()).toBeVisible();
  });
  await evidence.shots(page, "activity");

  await evidence.check("brushing the timeline filters /sessions by from and to", async () => {
    const chart = timeline.getByRole("img", { name: /Drag to select/ });
    // Certified steps are drawn as bars of the muted fill with a height.
    await expect
      .poll(() =>
        chart.evaluate(
          (svg) =>
            [...svg.querySelectorAll("rect.fill-muted")].filter(
              (rect) => Number(rect.getAttribute("height")) > 0,
            ).length,
        ),
      )
      .toBeGreaterThan(0);
    const box = await chart.boundingBox();
    if (box === null) throw new Error("the timeline chart has no box");
    await page.mouse.move(box.x + box.width * 0.6, box.y + 10);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width * 0.9, box.y + 10, { steps: 4 });
    await page.mouse.up();
    await expect(page).toHaveURL(/\/sessions\?(?=.*from=\d+)(?=.*to=\d+)/);
    await evidence.shots(page, "timeline-brushed");
    await timeline.getByRole("button", { name: "Clear" }).click();
    await expect(page).toHaveURL(/\/sessions$/);
  });

  await evidence.check("Add Host creates host-03", async () => {
    await explorer.getByRole("button", { name: "Add Host" }).click();
    const dialog = page.getByRole("dialog", { name: "Add Host" });
    await dialog.getByRole("textbox", { name: "Id" }).fill("host-03");
    await evidence.shots(page, "add-host-dialog");
    await dialog.getByRole("button", { name: "Add Host" }).click();
    await expect(hosts.getByRole("row", { name: "host-03" })).toBeVisible();
  });

  await evidence.check("importing a program raises a success toast", async () => {
    await explorer.locator('input[type="file"][accept=".wasm"]').setInputFiles(SEQUENTIAL_COUNT);
    await expect(page.getByText("Program imported").first()).toBeVisible();
  });

  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});
