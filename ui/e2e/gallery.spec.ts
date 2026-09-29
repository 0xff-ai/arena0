// Gallery suite: opens the dev-only /gallery page in both themes, exercises the
// interactive components through the browser, and leaves evidence under
// e2e/artifacts/gallery/: one full-page and one per-section screenshot per
// theme, a screenshot of every overlay, and summary.json listing each check
// that passed. Run with `pnpm playwright test --project gallery`.
import { expect, type Locator, type Page, test } from "@playwright/test";

const THEMES = ["light", "dark"] as const;

// The contract: one <section id> per component file, in page order.
const SECTIONS = [
  "button",
  "kbd",
  "badge",
  "status",
  "tooltip",
  "popover",
  "menu",
  "dialog",
  "toast",
  "field",
  "tabs",
  "tree",
  "section",
  "panel",
  "toolbar",
  "list",
  "table",
  "keyvalue",
  "empty",
  "palette",
  "fingerprint",
  "hashchip",
  "participant",
  "meter",
  "sparkline",
  "strip",
  "timeline",
  "ansi",
  "json",
  "schemaform",
];

// Fixed locale, zone and motion so screenshots do not vary between machines.
test.use({
  locale: "en-US",
  timezoneId: "UTC",
  reducedMotion: "reduce",
  permissions: ["clipboard-read", "clipboard-write"],
});

interface Fs {
  mkdirSync(path: string, options: { recursive: true }): void;
  writeFileSync(path: string, data: string): void;
}
// The project ships no Node type declarations, so `node:fs` is loaded through a
// computed specifier and given the two-function shape used here.
const FS_MODULE: string = "node:fs";
const loadFs = async () => (await import(FS_MODULE)) as Fs;

const passed: string[] = [];
let running = "page load";

/** Runs one named check as a report step and records its name once it passes. */
async function check(name: string, body: () => Promise<void>): Promise<void> {
  running = name;
  await test.step(name, body);
  passed.push(name);
}

/** The visible label of the Raw JSON switch; its input is visually hidden, so users click this. */
function rawToggle(form: Locator) {
  return form
    .locator("label")
    .filter({ has: form.page().getByRole("switch", { name: "Raw JSON" }) });
}

/** Overlays fade in; wait for every enter transition to finish so the screenshot is opaque. */
async function settled(page: Page) {
  await expect(page.locator("[data-entering]")).toHaveCount(0);
}

async function selectTheme(page: Page, theme: (typeof THEMES)[number]) {
  const group = page.getByRole("radiogroup", { name: "Theme" });
  await group.getByRole("radio", { name: theme === "dark" ? "Dark" : "Light" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
}

for (const theme of THEMES) {
  test(`gallery in the ${theme} theme`, async ({ page }, testInfo) => {
    const dir = `${testInfo.project.testDir}/artifacts/gallery`;
    const shots = `${dir}/${theme}`;
    (await loadFs()).mkdirSync(shots, { recursive: true });

    const problems: string[] = [];
    page.on("pageerror", (error) => problems.push(`[${running}] pageerror: ${error.message}`));
    page.on("console", (message) => {
      if (message.type() !== "error" && message.type() !== "warning") return;
      problems.push(`[${running}] ${message.type()}: ${message.text()}`);
    });

    await page.goto("/gallery");
    await expect(page.getByTestId("gallery")).toBeVisible();

    await check(`${theme}: the theme Segmented sets data-theme on <html>`, async () => {
      await selectTheme(page, theme);
    });

    await check(`${theme}: one section per component file, in order`, async () => {
      const ids = await page
        .locator("section[id]")
        .evaluateAll((sections) => sections.map((section) => section.id));
      expect(ids).toEqual(SECTIONS);
    });

    await check(`${theme}: nothing is wider than the viewport`, async () => {
      const widths = await page.evaluate(() => ({
        page: document.documentElement.scrollWidth,
        viewport: window.innerWidth,
        scroller: document.querySelector("[data-testid=gallery]")?.scrollWidth ?? 0,
        scrollerClient: document.querySelector("[data-testid=gallery]")?.clientWidth ?? 0,
      }));
      expect(widths.page).toBeLessThanOrEqual(widths.viewport);
      expect(widths.scroller).toBeLessThanOrEqual(widths.scrollerClient);
    });

    await check(`${theme}: full-page and per-section screenshots`, async () => {
      // The gallery scrolls inside its own box, so the viewport is grown to
      // the content height instead of asking for a full-page capture.
      const height = await page.evaluate(
        () => document.querySelector("[data-testid=gallery]")?.scrollHeight ?? 0,
      );
      await page.setViewportSize({ width: 1440, height });
      await page.screenshot({ path: `${shots}/full.png` });
      for (const id of SECTIONS) {
        await page.locator(`section#${id}`).screenshot({ path: `${shots}/${id}.png` });
      }
      await page.setViewportSize({ width: 1440, height: 900 });
    });

    await check(`${theme}: Segmented changes value`, async () => {
      const view = page.locator("#button").getByRole("radiogroup", { name: "View mode" });
      await expect(view.getByRole("radio", { name: "List" })).toBeChecked();
      await view.getByRole("radio", { name: "By host" }).click();
      await expect(view.getByRole("radio", { name: "By host" })).toBeChecked();
      await expect(view.getByRole("radio", { name: "List" })).not.toBeChecked();
      await expect(page.locator("#button")).toContainText("view: host");
    });

    await check(`${theme}: tooltip shows after hover with the label and shortcut`, async () => {
      const find = page.locator("#button").getByRole("button", { name: "Find", exact: true });
      await find.hover();
      const tooltip = page.getByRole("tooltip");
      await expect(tooltip).toBeVisible();
      await expect(tooltip).toContainText("Find");
      await expect(tooltip).toContainText(/Ctrl.?K|⌘K/);
      await settled(page);
      await page.screenshot({ path: `${shots}/overlay-tooltip.png` });
      await page.mouse.move(0, 0);
      await expect(tooltip).toBeHidden();
    });

    await check(`${theme}: HashChip tooltip holds the full hash and copy writes it`, async () => {
      const chip = page
        .locator("#hashchip")
        .getByRole("button", { name: /^[0-9a-f]{8}$/ })
        .first();
      await chip.hover();
      await expect(page.getByRole("tooltip")).toContainText(/[0-9a-f]{64}/);
      await page.mouse.move(0, 0);

      const copy = page.locator("#hashchip").getByRole("button", { name: "Copy hash" }).first();
      await copy.click();
      await expect(
        page.locator("#hashchip").getByRole("button", { name: "Copied" }).first(),
      ).toBeVisible();
      const shown = (await chip.textContent())?.trim() ?? "";
      const clipboard = await page.evaluate(() => navigator.clipboard.readText());
      expect(clipboard).toMatch(/^[0-9a-f]{64}$/);
      expect(clipboard.startsWith(shown)).toBe(true);
      // The check mark goes back to the copy icon on its own.
      await expect(
        page.locator("#hashchip").getByRole("button", { name: "Copy hash" }).first(),
      ).toBeVisible();
    });

    await check(`${theme}: menu opens, runs an item and closes`, async () => {
      await page.locator("#menu").getByRole("button", { name: "Actions" }).click();
      const menu = page.getByRole("menu");
      await expect(menu).toBeVisible();
      await expect(menu.getByRole("menuitem")).toHaveCount(4);
      await settled(page);
      await page.screenshot({ path: `${shots}/overlay-menu.png` });
      await menu.getByRole("menuitem", { name: "Copy link" }).click();
      await expect(menu).toBeHidden();
      await expect(page.locator("#menu")).toContainText("last action: Copy link");
    });

    await check(`${theme}: popover opens anchored to its button and Esc closes it`, async () => {
      await page.locator("#popover").getByRole("button", { name: "Filter sessions" }).click();
      const popover = page.getByRole("dialog", { name: "Filter sessions" });
      await expect(popover).toBeVisible();
      await settled(page);
      await page.screenshot({ path: `${shots}/overlay-popover.png` });
      await page.keyboard.press("Escape");
      await expect(popover).toBeHidden();
    });

    await check(
      `${theme}: dialogs open, confirm dialogs focus Cancel, results are reported`,
      async () => {
        const section = page.locator("#dialog");
        await section.getByRole("button", { name: "Medium" }).click();
        const dialog = page.getByRole("dialog", { name: "Import program (md)" });
        await expect(dialog).toBeVisible();
        await settled(page);
        await page.screenshot({ path: `${shots}/overlay-dialog.png` });
        await dialog.getByRole("button", { name: "Cancel" }).click();
        await expect(dialog).toBeHidden();

        await section.getByRole("button", { name: "Confirm (danger)" }).click();
        const alert = page.getByRole("alertdialog", { name: "Terminate session" });
        await expect(alert).toBeVisible();
        await expect(alert.getByRole("button", { name: "Cancel" })).toBeFocused();
        await settled(page);
        await page.screenshot({ path: `${shots}/overlay-confirm-danger.png` });
        await page.keyboard.press("Escape");
        await expect(alert).toBeHidden();
        await expect(section).toContainText("result: none");

        await section.getByRole("button", { name: "Confirm", exact: true }).click();
        await page
          .getByRole("alertdialog", { name: "Withdraw offer" })
          .getByRole("button", { name: "Withdraw" })
          .click();
        await expect(section).toContainText("result: withdrawn");
      },
    );

    await check(`${theme}: toasts appear bottom-right and can be dismissed`, async () => {
      const section = page.locator("#toast");
      await section.getByRole("button", { name: "ok", exact: true }).click();
      await section.getByRole("button", { name: "bad", exact: true }).click();
      const ok = page.getByText("Program imported");
      await expect(ok).toBeVisible();
      await expect(page.getByText("Answer rejected")).toBeVisible();
      await settled(page);
      await page.screenshot({ path: `${shots}/overlay-toast.png` });
      const box = await ok.boundingBox();
      expect(box).not.toBeNull();
      expect((box?.x ?? 0) > 720).toBe(true);
      expect((box?.y ?? 0) > 450).toBe(true);
      await page.getByRole("button", { name: "Dismiss" }).first().click();
      await page.getByRole("button", { name: "Dismiss" }).first().click();
      await expect(ok).toBeHidden();
    });

    await check(
      `${theme}: DocTabs pin on double-click, close on middle-click and on x, fixed tabs have no x`,
      async () => {
        const section = page.locator("#tabs");
        const chess = section.getByRole("tab", { name: /chess/ });
        await expect(chess.locator(".italic")).toHaveCount(1);
        await chess.dblclick();
        await expect(section).toContainText("last event: pin s2");
        await expect(chess.locator(".italic")).toHaveCount(0);

        await section.getByRole("tab", { name: /^receipt/ }).click({ button: "middle" });
        await expect(section).toContainText("last event: close r1");
        await expect(section.getByRole("tab", { name: /^receipt/ })).toHaveCount(0);

        const host = section.getByRole("tab", { name: /host-01/ });
        await host.hover();
        await host.locator("span[aria-hidden=true]").click();
        await expect(section).toContainText("last event: close h1");
        // Closing must not select the tab first.
        await expect(section).toContainText("Active document: s2");

        await expect(
          section.getByRole("tab", { name: /Sessions/ }).locator("span[aria-hidden=true]"),
        ).toHaveCount(0);
        await section.getByRole("tab", { name: /Offers/ }).click();
        await expect(section).toContainText("Active document: offers");
      },
    );

    await check(
      `${theme}: Tree selects on click, opens on Enter, expands with the arrow key`,
      async () => {
        const tree = page.locator("#tree");
        await tree.locator('[data-key="host-01"]').click();
        await expect(tree).toContainText("selected: host-01");
        await page.keyboard.press("Enter");
        await expect(tree).toContainText("opened: host-01");
        const host2 = tree.locator('[data-key="host-02"]');
        await host2.click();
        await expect(host2).toHaveAttribute("aria-expanded", "false");
        await page.keyboard.press("ArrowRight");
        await expect(host2).toHaveAttribute("aria-expanded", "true");
        await expect(tree.locator('[data-key="host-02/chess"]')).toBeVisible();
      },
    );

    await check(`${theme}: Section collapses and expands`, async () => {
      const section = page.locator("#section");
      await expect(section.getByText("host-01, host-02, host-03")).toBeVisible();
      await section.getByRole("button", { name: /Hosts/ }).click();
      await expect(section.getByText("host-01, host-02, host-03")).toBeHidden();
      await section.getByRole("button", { name: /Programs/ }).click();
      await expect(section.getByText("Collapsed until opened.")).toBeVisible();
    });

    await check(`${theme}: PanelHandle drags to resize the neighbouring panels`, async () => {
      const handle = page.locator("#panel").getByRole("separator").first();
      await handle.scrollIntoViewIfNeeded();
      const first = page.locator("#panel [data-panel]").first();
      const before = (await first.boundingBox())?.width ?? 0;
      const box = await handle.boundingBox();
      expect(box).not.toBeNull();
      const x = (box?.x ?? 0) + (box?.width ?? 0) / 2;
      const y = (box?.y ?? 0) + 40;
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.move(x + 90, y, { steps: 6 });
      await page.mouse.up();
      const after = (await first.boundingBox())?.width ?? 0;
      expect(after).toBeGreaterThan(before + 60);
    });

    await check(`${theme}: List selects a row and virtualizes 400 rows`, async () => {
      const list = page.locator("#list");
      const rendered = await list.getByRole("row").count();
      expect(rendered).toBeGreaterThan(5);
      expect(rendered).toBeLessThan(60);
      await list.getByRole("row").nth(5).click();
      await expect(list).toContainText("selected: s5");
    });

    await check(`${theme}: DataTable selects a row and shows its empty state`, async () => {
      const table = page.locator("#table");
      const row = table.locator('[data-key="s4"]');
      await row.click();
      await expect(row).toHaveAttribute("aria-selected", "true");
      await expect(table.getByText("No receipts")).toBeVisible();
    });

    await check(
      `${theme}: TimelineChart brushes a range on drag and clears it on click`,
      async () => {
        const chart = page.locator("#timeline").getByRole("img").first();
        await chart.scrollIntoViewIfNeeded();
        const box = await chart.boundingBox();
        expect(box).not.toBeNull();
        const left = box?.x ?? 0;
        const top = (box?.y ?? 0) + 30;
        await page.mouse.move(left + 200, top);
        await page.mouse.down();
        await page.mouse.move(left + 480, top, { steps: 8 });
        await page.mouse.up();
        await expect(page.getByTestId("brush-readout")).toContainText("min selected");
        await page
          .locator("#timeline")
          .screenshot({ path: `${shots}/interaction-timeline-brush.png` });
        await page.mouse.click(left + 700, top);
        await expect(page.getByTestId("brush-readout")).toContainText("brush: none");
      },
    );

    await check(
      `${theme}: palette filters on label and keywords, Enter runs the action and closes`,
      async () => {
        await page.getByTestId("open-palette").click();
        const palette = page.getByRole("dialog", { name: "Command palette" });
        await expect(palette).toBeVisible();
        await expect(palette.getByRole("menuitem")).toHaveCount(8);
        await settled(page);
        await page.screenshot({ path: `${shots}/overlay-palette.png` });

        await palette.getByRole("searchbox").fill("chess");
        await expect(palette.getByRole("menuitem")).toHaveCount(1);
        await settled(page);
        await page.screenshot({ path: `${shots}/overlay-palette-filtered.png` });
        await page.keyboard.press("Enter");
        await expect(palette).toBeHidden();
        await expect(page.getByTestId("last-action")).toContainText("chess");

        await page.getByTestId("open-palette").click();
        await palette.getByRole("searchbox").fill("rps");
        await expect(palette.getByRole("menuitem")).toHaveCount(1);
        await expect(palette.getByRole("menuitem")).toContainText("rock-paper-scissors");
        await page.keyboard.press("Escape");
        await expect(palette).toBeHidden();
        await expect(page.getByTestId("last-action")).toContainText("chess");
      },
    );

    await check(
      `${theme}: SchemaForm shows an issue for an out-of-range integer and clears it when fixed`,
      async () => {
        const form = page.getByTestId("params-form");
        const rounds = form.getByRole("textbox", { name: /Rounds/ });
        await rounds.fill("500");
        await rounds.press("Enter");
        const issue = form.getByText("500 is greater than 50.");
        await expect(issue).toBeVisible();
        await expect(page.getByTestId("issue-list")).toContainText("/rounds");
        await page
          .locator("#schemaform")
          .screenshot({ path: `${shots}/interaction-schemaform-invalid.png` });
        await rounds.fill("10");
        await rounds.press("Enter");
        await expect(issue).toBeHidden();
        await expect(page.getByTestId("issue-list")).toContainText("valid");
      },
    );

    await check(
      `${theme}: SchemaForm reports a missing required field under that field`,
      async () => {
        const form = page.getByTestId("params-form");
        const item = form.getByRole("textbox", { name: /Item/ });
        await item.fill("");
        await expect(form.getByText("is required")).toBeVisible();
        await item.fill("widget");
        await expect(form.getByText("is required")).toBeHidden();
      },
    );

    await check(`${theme}: SchemaForm array repeater adds and removes rows`, async () => {
      const form = page.getByTestId("params-form");
      await expect(form.getByRole("textbox", { name: /Name/ })).toHaveCount(1);
      await form.getByRole("button", { name: "Add item" }).click();
      await expect(form.getByRole("textbox", { name: /Name/ })).toHaveCount(2);
      await form.getByRole("button", { name: "Remove bidders 2" }).click();
      await expect(form.getByRole("textbox", { name: /Name/ })).toHaveCount(1);
    });

    await check(
      `${theme}: SchemaForm Raw JSON edits the same value; bad text leaves it unchanged`,
      async () => {
        const form = page.getByTestId("params-form");
        await rawToggle(form).click();
        const editor = form.getByRole("textbox", { name: "Raw JSON" });
        await expect(editor).toHaveValue(/"item": "widget"/);

        await editor.fill('{"item": "gadget", "currency"');
        await expect(form.getByRole("alert")).toBeVisible();
        await expect(page.getByTestId("value-view")).not.toContainText("gadget");

        await editor.fill('{"item":"gadget","currency":"usd","rounds":5}');
        await expect(form.getByRole("alert")).toBeHidden();
        await expect(page.getByTestId("issue-list")).toContainText("valid");
        await expect(page.getByTestId("value-view")).toContainText("gadget");

        await rawToggle(form).click();
        await expect(form.getByRole("textbox", { name: /Item/ })).toHaveValue("gadget");
      },
    );

    await check(`${theme}: AnsiText renders no escape or control characters`, async () => {
      const texts = await page
        .locator("#ansi pre:not([data-testid=ansi-raw])")
        .evaluateAll((elements) => elements.map((element) => element.textContent ?? ""));
      expect(texts.length).toBeGreaterThanOrEqual(6);
      for (const text of texts) {
        expect(text).not.toContain("\u001b");
        expect(text).not.toMatch(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/);
      }
      const hostile = texts.find((text) => text.includes("green survives")) ?? "";
      expect(hostile).toBe("line oneline two link text end green survives");
      // The 16-colour codes become theme classes, not inline colours.
      await expect(page.locator("#ansi .text-ansi-2").first()).toBeVisible();
      // 256-colour and true-colour codes become inline colours.
      await expect(page.locator('#ansi span[style*="255, 105, 180"]')).toHaveCount(1);
    });

    await check(`${theme}: JsonView collapses deep containers behind a toggle`, async () => {
      const first = page.locator("#json .font-mono").first();
      await expect(first.getByText("4 keys").first()).toBeVisible();
      await first.getByRole("button", { name: "Expand" }).first().click();
      await expect(first.getByText('"index"').first()).toBeVisible();
    });

    await check(`${theme}: no page errors, console errors or warnings`, async () => {
      expect(problems).toEqual([]);
    });
  });
}

test.afterAll(async ({}, testInfo) => {
  const dir = `${testInfo.project.testDir}/artifacts/gallery`;
  const fs = await loadFs();
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    `${dir}/summary.json`,
    `${JSON.stringify({ suite: "gallery", sections: SECTIONS, passed }, null, 2)}\n`,
  );
});
