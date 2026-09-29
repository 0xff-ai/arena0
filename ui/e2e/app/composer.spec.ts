// Live composer, launch form and program, Host and receipt documents against a
// real `arena0 ui`. Evidence goes to e2e/artifacts/app-composer/.
import { readFileSync } from "node:fs";
import type { Locator, Page } from "@playwright/test";
import { type Arena, Evidence, expect, openApp, test, watchConsole } from "../harness";

const evidence = new Evidence("app-composer");
test.afterAll(() => evidence.write());

// The suites share one daemon per worker and build on each other: the launch
// test produces the session whose receipt the receipt test reads.
test.describe.configure({ mode: "serial" });

async function programHash(arena: Arena, name: string): Promise<string> {
  const listing: { programs: { name: string; program_id: string }[] } = JSON.parse(
    await arena.cli(["--json", "program", "list"]),
  );
  const program = listing.programs.find((p) => p.name === name);
  if (program === undefined) throw new Error(`no bundled program ${name}`);
  return program.program_id;
}

/**
 * Delay the gateway's row updates on demand. The composer shows "Accepted"
 * between the daemon taking an answer and the callout row going away, which
 * on a local daemon is a few milliseconds; holding the rows makes that state
 * observable. Replies still pass, so the answer itself is the real call.
 */
async function rowsHold(page: Page): Promise<{ hold(): void; release(): void }> {
  let held: string[] | null = null;
  let toPage: ((frame: string) => void) | null = null;
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    toPage = (frame) => ws.send(frame);
    ws.onMessage((frame) => server.send(frame));
    server.onMessage((frame) => {
      if (held !== null && typeof frame === "string" && frame.startsWith('{"t":"rows"')) {
        held.push(frame);
      } else ws.send(frame);
    });
  });
  return {
    hold: () => {
      held = [];
    },
    release: () => {
      const frames = held ?? [];
      held = null;
      for (const frame of frames) toPage?.(frame);
    },
  };
}

const composerOf = (page: Page): Locator => page.getByRole("region", { name: "Composer" });

// An empty Needs input list is itself a row ("Nothing needs you"), so pick host-01's callout row (host-02 plays a strategy that answers its own callouts).
const needsRow = (page: Page): Locator =>
  page
    .getByRole("grid", { name: "Needs input" })
    .getByRole("row")
    .filter({ hasText: "ChooseMove" })
    .filter({ hasText: "host-01" })
    .first();

/** Open the oldest callout of a seat from the Needs input list, as a user does. */
async function openFirstCallout(page: Page): Promise<void> {
  await needsRow(page).dblclick();
  await expect(composerOf(page)).toBeVisible();
}

test("launch a session from its program, answer through the composer until it completes", async ({
  page,
  arena,
}) => {
  const problems = watchConsole(page);
  const rows = await rowsHold(page);
  const hash = await programHash(arena, "rock-paper-scissors");
  // `openApp` sets the URL's pathname, which would escape a query string.
  await openApp(page, arena, `/programs/${hash}`);
  await page.getByRole("main").getByRole("button", { name: "New session" }).click();
  await expect(page).toHaveURL(/launch=true/);

  await evidence.check("the launch form offers two participants and two Host seats", async () => {
    await expect(page.getByRole("textbox", { name: "Participants" })).toHaveValue("2");
    await expect(page.getByRole("button", { name: /Host for seat 1/ })).toContainText("host-01");
    await expect(page.getByRole("button", { name: /Host for seat 2/ })).toContainText("host-02");
  });

  const seat1 = page.getByRole("radiogroup", { name: "Driver for seat 1" });
  const seat2 = page.getByRole("radiogroup", { name: "Driver for seat 2" });
  await seat1.getByRole("radio", { name: "You" }).click();
  await seat2.getByRole("radio", { name: "Strategy" }).click();
  await page.getByRole("button", { name: /Strategy for seat 2/ }).click();
  await page.getByRole("option", { name: /first-allowed/ }).click();
  await evidence.shots(page, "launch-form");

  await page.getByRole("button", { name: "Launch" }).click();
  await evidence.check("Launch opens the new session and the composer for host-01", async () => {
    await expect(page).toHaveURL(/\/s\//);
    await expect(composerOf(page)).toBeVisible();
    await expect(composerOf(page)).toContainText("host-01");
  });
  await evidence.shots(page, "composer-open");

  const receipts = page.getByRole("tab", { name: /Receipts\s*2/ });
  let answers = 0;
  await evidence.check("answering through the composer accepts, closes and toasts", async () => {
    for (;;) {
      await expect(composerOf(page).or(receipts)).toBeVisible({ timeout: 30_000 });
      if (await receipts.isVisible()) break;
      await composerOf(page).getByRole("radio").first().click();
      if (answers === 0) rows.hold();
      await composerOf(page)
        .getByRole("button", { name: /Submit/ })
        .click();
      if (answers === 0) {
        await expect(composerOf(page)).toContainText("Accepted — waiting for agreement");
        await evidence.shots(page, "accepted");
        rows.release();
      }
      await expect(composerOf(page)).toBeHidden();
      // Toasts stay for several rounds, so only the first one is unambiguous.
      if (answers === 0) await expect(page.getByText(/Answered · step \d+/)).toBeVisible();
      answers += 1;
      // The next round's callout appears in Needs input; open it like a user would.
      // The last round is followed by the end handshake with the other Host, which takes seconds.
      await expect(needsRow(page).or(receipts)).toBeVisible({ timeout: 30_000 });
      if (await receipts.isVisible()) break;
      await openFirstCallout(page);
    }
  });
  await evidence.check("the session completed after several answers", async () => {
    expect(answers).toBeGreaterThan(1);
  });
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("the receipt document summarises, verifies and exports the completed session", async ({
  page,
  arena,
}) => {
  const problems = watchConsole(page);
  const listing: { receipts: { receipt_id: string; session_id: string }[] } = JSON.parse(
    await arena.cli(["--json", "--host", "host-01", "receipt", "list"]),
  );
  const [receipt] = listing.receipts;
  if (receipt === undefined) throw new Error("the launch test left no receipt on host-01");
  await openApp(page, arena, `/receipts/host-01/${receipt.receipt_id}`);

  const main = page.getByRole("main");
  await evidence.check("the summary names the program, Host and provenance", async () => {
    await expect(main.getByRole("heading", { name: "Receipt", exact: true })).toBeVisible();
    await expect(main).toContainText("Rock-Paper-Scissors");
    await expect(main).toContainText("host-01");
    await expect(main).toContainText("produced");
  });
  await evidence.check("Verify lists the checks that passed", async () => {
    await main.getByRole("button", { name: "Verify", exact: true }).click();
    const checks = main.getByRole("list", { name: "Verification checks" });
    await expect(checks).toContainText("Structure and signatures verified");
    await expect(checks).toContainText("Program matches");
    await expect(checks).toContainText("Session matches");
    await expect(checks).toContainText("Termination: completed");
    await expect(checks).not.toContainText("failed");
  });
  await evidence.check("the artifact is shown as JSON", async () => {
    await expect(main.getByRole("heading", { name: "Artifact" })).toBeVisible();
    await expect(main.getByText(/"terminal": "Completed"/)).toBeVisible();
    await expect(main.getByText(/"trace": \[/)).toBeVisible();
  });
  await evidence.shots(page, "receipt-doc");
  await evidence.check("Export downloads the receipt as <id>.json", async () => {
    const download = page.waitForEvent("download");
    await main.getByRole("button", { name: "Export" }).click();
    const file = await download;
    expect(file.suggestedFilename()).toBe(`${receipt.receipt_id}.json`);
    const path = await file.path();
    const saved = JSON.parse(readFileSync(path, "utf8"));
    expect(JSON.stringify(saved)).toContain('"terminal":"Completed"');
  });
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("an invalid answer is blocked and a draft survives switching tabs", async ({
  page,
  arena,
}) => {
  const problems = watchConsole(page);
  // No Host is bound to a strategy, so the offer callouts stay open for the page.
  // (A strategy answering a WorkerOffer makes the program abort the session.)
  arena.spawn([
    "--json",
    "launch",
    "contract-net",
    "--param",
    "target_size=2",
    "--param",
    'tasks=[{"name":"t","capability":"c"}]',
  ]);
  await openApp(page, arena, "/sessions");
  const row = page
    .getByRole("grid", { name: "Needs input" })
    .getByRole("row")
    .filter({ hasText: "SubmitOffer" });
  // The launch runs in the background: it negotiates before the callout opens.
  await expect(row).toHaveCount(1, { timeout: 30_000 });
  await row.dblclick();
  const composer = composerOf(page);
  await expect(composer).toBeVisible();
  await evidence.shots(page, "composer-schema-form");

  const capacity = composer.getByRole("textbox", { name: /capacity/i });
  await evidence.check(
    "Mod+Enter submits the number typed, not the last committed one",
    async () => {
      // 70000 exceeds the u16 capacity. Typing without leaving the field and pressing
      // Mod+Enter only reports it if the shortcut commits the field first.
      await capacity.fill("70000");
      await capacity.press("ControlOrMeta+Enter");
      await expect(composer.getByRole("alert").filter({ hasText: /65535/ })).toBeVisible();
    },
  );
  await evidence.check("schema validation blocks the invalid answer client-side", async () => {
    await expect(composer).not.toContainText("Accepted");
    await expect(composer.getByRole("button", { name: /Submit/ })).toBeEnabled();
    await expect(row).toHaveCount(1);
  });
  // The form scrolls inside the dock; bring the issue into the frame for the screenshot.
  await composer.getByRole("alert").filter({ hasText: /65535/ }).scrollIntoViewIfNeeded();
  await evidence.shots(page, "composer-invalid");

  await evidence.check("the draft is still there after switching tabs and back", async () => {
    await page.getByRole("tab", { name: /^Programs/ }).click();
    await expect(page).toHaveURL(/\/programs$/);
    await page.getByRole("tab", { name: /Contract Net Allocation/ }).click();
    await expect(page).toHaveURL(/\/s\//);
    await expect(capacity).toHaveValue("70000");
  });
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("the Host document shows identity, programs and an empty blob table", async ({
  page,
  arena,
}) => {
  const problems = watchConsole(page);
  await openApp(page, arena, "/hosts/host-01");
  const main = page.getByRole("main");

  await evidence.check("identity lists the peer, user agent and counts", async () => {
    await expect(main.getByRole("heading", { name: "host-01", level: 1 })).toBeVisible();
    await expect(main).toContainText("peer id");
    await expect(main).toContainText("user agent");
    await expect(main.getByText("online", { exact: true })).toBeVisible();
  });
  await evidence.check("the programs of this Host are listed", async () => {
    await expect(main.getByRole("button", { name: "Chess" })).toBeVisible();
    await expect(main.getByRole("button", { name: "Rock-Paper-Scissors" })).toBeVisible();
  });
  await evidence.check("the blob table shows its empty state", async () => {
    await expect(main.getByText("No blobs on host-01")).toBeVisible();
  });
  await evidence.shots(page, "host-doc");
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("the program document lists phases, callouts and queries", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await openApp(page, arena, `/programs/${await programHash(arena, "chess")}`);
  const main = page.getByRole("main");

  await evidence.check("the header names the program, version and Hosts", async () => {
    await expect(main.getByRole("heading", { name: "Chess", level: 1 })).toBeVisible();
    await expect(main).toContainText("v1.0.0");
    await expect(main).toContainText("host-01");
    await expect(main).toContainText("host-02");
  });
  await evidence.check("phases are listed with the default badge", async () => {
    const setup = main.getByRole("listitem").filter({ hasText: "Setup" });
    await expect(setup).toContainText("default");
    await expect(main.getByRole("listitem").filter({ hasText: "Playing" })).toContainText(
      "Game in progress",
    );
  });
  await evidence.check("callouts and queries are listed", async () => {
    await expect(main.getByRole("heading", { name: /Callouts/ })).toBeVisible();
    await expect(main.getByText("MakeMove")).toBeVisible();
    await expect(main.getByRole("heading", { name: /Queries/ })).toBeVisible();
    await expect(main.getByText("No queries.")).toBeVisible();
  });
  await evidence.shots(page, "program-doc");
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});
