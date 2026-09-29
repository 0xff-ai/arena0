// Live session document suite: the session document, inspector and verify
// card against a real `arena0 ui`. Evidence goes to e2e/artifacts/app-session/.
import { readFileSync } from "node:fs";
import type { Page } from "@playwright/test";
import { type Arena, Evidence, expect, openApp, seed, test, watchConsole } from "../harness";

const evidence = new Evidence("app-session");
test.use({ suite: "app-session" });
test.afterAll(() => evidence.write());
test.describe.configure({ mode: "serial" });

/**
 * host-02 is the user: it answers its open callout with the first legal move,
 * then waits for the next one. Which side moves first varies per session, but
 * after this the position has at least steps 0 to 2 whoever started.
 */
async function playOneMove(arena: Arena, execId: string): Promise<void> {
  const host02 = (...args: string[]) => arena.cli(["--json", "--host", "host-02", ...args]);
  const callout: { Callout: { pending_id: string; context: { legal_moves: string } } } = JSON.parse(
    await host02("exec", "next", execId),
  );
  const move = callout.Callout.context.legal_moves.split(",")[0]?.trim();
  await host02(
    "exec",
    "submit",
    execId,
    "--pending-id",
    callout.Callout.pending_id,
    "--answer",
    JSON.stringify(move),
  );
  // Blocks until the reply is certified and the user's next callout opens.
  await host02("exec", "next", execId);
}

test.beforeAll(async ({ arena }) => {
  await seed.completed(arena, "rock-paper-scissors");
  const { execId } = await seed.awaitingYou(arena, "chess");
  await playOneMove(arena, execId);
});

/** Opens a session document through the palette, by the program's display name. */
async function openSession(page: Page, program: string): Promise<string> {
  await page.keyboard.press("Control+K");
  await page.getByRole("searchbox", { name: "Type a command or search…" }).fill(program);
  // Palette rows carry no accessible marker of their kind; the session rows are the ones keyed `session:`.
  await page.locator(`[role="menuitem"][data-key^="session:"]`, { hasText: program }).click();
  await expect(page).toHaveURL(/\/s\//);
  return page.url();
}

const inspector = (page: Page) => page.getByRole("complementary", { name: "Inspector" });

test("RPS: header, front matter, steps and step selection", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await openApp(page, arena, "/sessions");
  const url = await openSession(page, "Rock-Paper-Scissors");

  await evidence.check("the header names the program and the state is completed", async () => {
    await expect(page.getByRole("heading", { name: "Rock-Paper-Scissors" })).toBeVisible();
    await expect(
      page.getByRole("main").getByText("completed", { exact: true }).first(),
    ).toBeVisible();
  });
  await evidence.check(
    "front matter lists the session id and each participant's Host",
    async () => {
      await expect(page.getByText("session id")).toBeVisible();
      await expect(page.getByText("host-01").first()).toBeVisible();
      await expect(page.getByText("host-02").first()).toBeVisible();
    },
  );
  await evidence.check(
    "Steps lists 13 lines with the terminal line and alignment on every step",
    async () => {
      const steps = page.getByRole("row", { name: /^Step \d+$/ });
      await expect(steps).toHaveCount(13);
      await expect(steps.first()).toContainText("session started");
      await expect(steps.last()).toContainText("end");
      await expect(page.getByRole("img", { name: "Hosts agree" })).toHaveCount(13);
    },
  );
  await evidence.shots(page, "steps");

  await page.goto(`${url}?step=5`);
  await evidence.check("?step=5 selects step 5 and the inspector decodes its event", async () => {
    await expect(page.getByRole("row", { name: "Step 5" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(inspector(page).getByText("Step 5", { exact: true })).toBeVisible();
    await expect(inspector(page).getByText("Event", { exact: true })).toBeVisible();
  });
  await evidence.shots(page, "step-selected");
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("chess: view blocks, text mode, step slider and host comparison", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await openApp(page, arena, "/sessions");
  const url = await openSession(page, "Chess");
  await expect(page.getByRole("row", { name: "Step 2" })).toBeVisible();
  await page.goto(`${url}?sub=view`);

  const board = page.getByRole("main").getByRole("grid");
  const squares = async () => board.getByRole("gridcell").allInnerTexts();
  await evidence.check(
    "the board block renders 64 cells with rank and file labels and a To move fact",
    async () => {
      await expect(board.getByRole("gridcell")).toHaveCount(64);
      await expect(board.getByRole("rowheader")).toHaveText(
        ["1", "2", "3", "4", "5", "6", "7", "8"].reverse(),
      );
      await expect(board.getByRole("columnheader")).toHaveText([
        "a",
        "b",
        "c",
        "d",
        "e",
        "f",
        "g",
        "h",
      ]);
      await expect(page.getByText("To move")).toBeVisible();
    },
  );
  const latest = await squares();
  await evidence.shots(page, "view-blocks");

  await evidence.check("Text mode shows the ANSI board instead of the blocks", async () => {
    await page.getByRole("radio", { name: "Text" }).click();
    await expect(board).toHaveCount(0);
    await expect(page.locator("pre", { hasText: /a\s+b\s+c\s+d\s+e\s+f\s+g\s+h/ })).toBeVisible();
  });
  await evidence.shots(page, "view-text");
  await page.getByRole("radio", { name: "Both" }).click();

  await evidence.check(
    "the step slider at 0 shows the initial position and puts step=0 in the URL",
    async () => {
      await page.getByRole("slider", { name: "Step" }).focus();
      await page.keyboard.press("Home");
      await expect(page).toHaveURL(/step=0/);
      await expect(async () => expect(await squares()).not.toEqual(latest)).toPass();
    },
  );

  await page.goto(`${url}?sub=view&host=host-01&compare=host-02`);
  await evidence.check("comparing host-01 with host-02 shows both panes", async () => {
    await expect(page.getByRole("main").getByRole("grid")).toHaveCount(2);
  });
  await evidence.shots(page, "compare");
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("chess: negotiation and query tabs", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await openApp(page, arena, "/sessions");
  const url = await openSession(page, "Chess");

  await page.goto(`${url}?sub=negotiation`);
  await evidence.check("Negotiation shows each Host's activation facts", async () => {
    for (const host of ["host-01", "host-02"]) {
      const section = page.getByRole("group", { name: `Activation on ${host}` });
      await expect(section.getByText("committed")).toBeVisible();
      await expect(section.getByText("offer", { exact: true })).toBeVisible();
      await expect(
        section.getByRole("list", { name: "Participants and tickets" }).getByRole("listitem"),
      ).toHaveCount(2);
    }
  });
  await evidence.shots(page, "negotiation");

  await page.goto(`${url}?sub=query`);
  // No bundled program declares a query, so the Run path has no live program to run against.
  await evidence.check("Query says the program declares no queries", async () => {
    await expect(page.getByText("This program declares no queries")).toBeVisible();
  });
  await evidence.shots(page, "query-empty");
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});

test("RPS: evidence verifies and exports; chess terminates", async ({ page, arena }) => {
  const problems = watchConsole(page);
  await openApp(page, arena, "/sessions");
  const rps = await openSession(page, "Rock-Paper-Scissors");
  await page.goto(`${rps}?sub=evidence`);

  await evidence.check("each Host shows its receipt and Verify passes every check", async () => {
    for (const host of ["host-01", "host-02"]) {
      const section = page.getByRole("region", { name: `Evidence on ${host}` });
      await section.getByRole("button", { name: "Verify" }).click();
      const checks = section
        .getByRole("list", { name: "Verification checks" })
        .getByRole("listitem");
      await expect(checks.first()).toBeVisible();
      await expect(checks.filter({ hasText: "failed" })).toHaveCount(0);
      expect(await checks.filter({ hasText: "passed" }).count()).toBe(await checks.count());
    }
  });
  await evidence.shots(page, "evidence-verified");

  await evidence.check("Export downloads <receipt id>.json with the artifact", async () => {
    const section = page.getByRole("region", { name: "Evidence on host-01" });
    const shortId = await section
      .locator("button", { hasText: /^[0-9a-f]{8}$/ })
      .first()
      .innerText();
    const download = page.waitForEvent("download");
    await section.getByRole("button", { name: "Export" }).click();
    const file = await download;
    expect(file.suggestedFilename()).toMatch(new RegExp(`^${shortId}[0-9a-f]{56}\\.json$`));
    const artifact: unknown = JSON.parse(readFileSync(await file.path(), "utf8"));
    expect(artifact).toBeInstanceOf(Object);
  });

  const chess = await openSession(page, "Chess");
  await page.goto(chess);
  await page.getByRole("button", { name: "More" }).click();
  await page.getByRole("menuitem", { name: "Terminate" }).click();
  await evidence.shots(page, "terminate-confirm");
  await evidence.check(
    "Terminate from More, after confirming, ends the session as aborted",
    async () => {
      await page.getByRole("alertdialog").getByRole("button", { name: "Terminate" }).click();
      await expect(
        page.getByRole("main").getByText("aborted", { exact: true }).first(),
      ).toBeVisible();
    },
  );
  await evidence.check("no console errors or warnings", async () => {
    expect(problems()).toEqual([]);
  });
});
