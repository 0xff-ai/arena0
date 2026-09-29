// Live end-to-end harness: one real daemon and gateway per Playwright worker.
//
// `arena0 ui` runs against a fresh ARENA0_HOME with its default two Hosts and
// the bundled programs. Tests seed state through the real CLI (`arena0 run`,
// `arena0 launch`) against that same home, then drive the page. Nothing is
// faked. The binary comes from ARENA0_BIN, else ../target/debug/arena0; it
// must have been built after `pnpm build` so it embeds the current UI.

import { type ChildProcess, execFile, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test as base, expect, type Page } from "@playwright/test";

export { expect };

const BIN = process.env.ARENA0_BIN ?? resolve(import.meta.dirname, "../../target/debug/arena0");
const ARTIFACTS = resolve(import.meta.dirname, "artifacts");

export interface Arena {
  /** The page URL, token in the fragment. */
  url: string;
  home: string;
  /** Run the CLI against this daemon; resolves with stdout, rejects on a non-zero exit. */
  cli(args: string[]): Promise<string>;
  /** Start a CLI command that keeps running (e.g. `launch`); killed at teardown. */
  spawn(args: string[]): void;
}

function startArena(): Promise<{ arena: Arena; stop(): Promise<void> }> {
  const home = mkdtempSync(join(tmpdir(), "arena0-e2e-"));
  const env = { ...process.env, ARENA0_HOME: home };
  const background: ChildProcess[] = [];
  const ui = spawn(BIN, ["--json", "ui", "--no-open", "--port", "0"], {
    env,
    stdio: ["ignore", "pipe", "inherit"],
  });
  const exited = new Promise<void>((done) => ui.once("exit", () => done()));

  const stop = async () => {
    for (const child of background) child.kill("SIGINT");
    // SIGINT is the gateway's Ctrl-C: it closes sockets and stops the daemon it started.
    ui.kill("SIGINT");
    await exited;
    rmSync(home, { recursive: true, force: true });
  };

  return new Promise((ready, fail) => {
    let out = "";
    ui.stdout?.on("data", (chunk: Buffer) => {
      out += chunk.toString();
      const line = out.split("\n").find((l) => l.startsWith("{"));
      if (line === undefined) return;
      const { url } = JSON.parse(line) as { url: string };
      ready({
        arena: {
          url,
          home,
          cli: (args) =>
            new Promise((done, reject) => {
              execFile(BIN, args, { env, timeout: 120_000 }, (error, stdout, stderr) =>
                error ? reject(new Error(`arena0 ${args.join(" ")}: ${stderr}`)) : done(stdout),
              );
            }),
          spawn: (args) => {
            background.push(spawn(BIN, args, { env, stdio: "ignore" }));
          },
        },
        stop,
      });
    });
    ui.once("exit", (code) =>
      fail(new Error(`arena0 ui exited (${code}) before printing its URL`)),
    );
  });
}

export const test = base.extend<object, { arena: Arena }>({
  arena: [
    async ({}, use) => {
      const { arena, stop } = await startArena();
      await use(arena);
      await stop();
    },
    { scope: "worker" },
  ],
});

test.use({ locale: "en-US", timezoneId: "UTC", viewport: { width: 1440, height: 900 } });

/** Page errors and console errors or warnings seen since the call. */
export function watchConsole(page: Page): () => string[] {
  const problems: string[] = [];
  page.on("pageerror", (error) => problems.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") {
      problems.push(`${message.type()}: ${message.text()}`);
    }
  });
  return () => problems;
}

/** Open the workspace at `path` and wait until the gateway snapshot is live. */
export async function openApp(page: Page, arena: Arena, path = "/"): Promise<void> {
  const url = new URL(arena.url);
  url.pathname = path;
  await page.goto(url.toString());
  await expect(page.getByTestId("connection")).toHaveText(/live/, { timeout: 30_000 });
}

/** Seeds through the real CLI. Program names are the bundled catalog names. */
export const seed = {
  /** Run a program to completion with built-in strategies on both Hosts. */
  async completed(arena: Arena, program: string): Promise<void> {
    await arena.cli([
      "--json",
      "run",
      program,
      "--builtin",
      "host-01=first-allowed",
      "--builtin",
      "host-02=first-allowed",
      "--no-tui",
    ]);
  },
  /**
   * Start a session where host-01 plays a built-in strategy and host-02 is
   * left to the user: host-02's callouts stay open for the page to answer.
   */
  awaitingYou(arena: Arena, program: string): void {
    arena.spawn(["--json", "launch", program, "--builtin", "host-01=first-allowed"]);
  },
};

/** Evidence for one suite: screenshots in both themes and the checks that passed. */
export class Evidence {
  readonly #dir: string;
  readonly #passed: string[] = [];

  constructor(suite: string) {
    this.#dir = join(ARTIFACTS, suite);
  }

  /** Screenshot the page in light then dark to `<suite>/<theme>/<name>.png`. */
  async shots(page: Page, name: string): Promise<void> {
    for (const theme of ["light", "dark"] as const) {
      await page.evaluate((value) => {
        localStorage.setItem("arena0.theme", value);
        // The preference store listens for `storage`, which only other tabs fire.
        window.dispatchEvent(new StorageEvent("storage", { key: "arena0.theme" }));
      }, theme);
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      mkdirSync(join(this.#dir, theme), { recursive: true });
      await page.screenshot({ path: join(this.#dir, theme, `${name}.png`) });
    }
  }

  /** Run a named check as a test step; it is recorded only if it passes. */
  async check(name: string, body: () => Promise<void>): Promise<void> {
    await test.step(name, body);
    this.#passed.push(name);
  }

  /** Write `<suite>/summary.json`. Call from `test.afterAll`. */
  write(): void {
    mkdirSync(this.#dir, { recursive: true });
    writeFileSync(
      join(this.#dir, "summary.json"),
      `${JSON.stringify({ passed: this.#passed }, null, 2)}\n`,
    );
  }
}
