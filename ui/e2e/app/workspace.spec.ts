// Live workspace shell: the workspace against a real `arena0 ui`. Evidence goes to
// e2e/artifacts/app-workspace/.
import { Evidence, expect, openApp, seed, test, watchConsole } from "../harness";

const evidence = new Evidence("app-workspace");
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
