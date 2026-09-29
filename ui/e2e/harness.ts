// Live end-to-end harness: one real daemon and gateway per Playwright worker.
//
// `arena0 ui` runs against a fresh ARENA0_HOME with its default two Hosts and
// the bundled programs. Tests seed state through the real CLI (`arena0 run`,
// `arena0 launch`) against that same home, then drive the page. Nothing is
// faked. The binary comes from ARENA0_BIN, else ../target/debug/arena0.
//
// The page comes from one of two places:
// - default: a Vite dev server per worker, proxying /ws to that worker's
//   gateway (`--dev-origin`), so suites test the current source without
//   rebuilding the binary;
// - ARENA0_E2E_EMBEDDED=1: the UI embedded in the binary, as users get it
//   (`just test-ui` builds the UI and the binary first).

import { type ChildProcess, execFile, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test as base, expect, type Page } from "@playwright/test";

export { expect };

const UI_DIR = resolve(import.meta.dirname, "..");
const BIN = process.env.ARENA0_BIN ?? resolve(UI_DIR, "../target/debug/arena0");
const ARTIFACTS = resolve(import.meta.dirname, "artifacts");
const EMBEDDED = process.env.ARENA0_E2E_EMBEDDED === "1";

export interface Arena {
  /** The page URL, token in the fragment. */
  url: string;
  home: string;
  /** Run the CLI against this daemon; resolves with stdout, rejects on a non-zero exit. */
  cli(args: string[]): Promise<string>;
  /** Start a CLI command that keeps running (e.g. `launch`); killed at teardown. */
  spawn(args: string[]): void;
}

function freePort(): Promise<number> {
  return new Promise((done, fail) => {
    const server = createServer();
    server.once("error", fail);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = typeof address === "object" && address !== null ? address.port : 0;
      server.close(() => done(port));
    });
  });
}

/** Resolve with the first stdout line starting with `{`, parsed. */
function firstJsonLine(child: ChildProcess, what: string): Promise<{ url: string }> {
  return new Promise((done, fail) => {
    let out = "";
    child.stdout?.on("data", (chunk: Buffer) => {
      out += chunk.toString();
      const line = out.split("\n").find((l) => l.startsWith("{"));
      if (line !== undefined) done(JSON.parse(line) as { url: string });
    });
    child.once("exit", (code) => fail(new Error(`${what} exited (${code}) before it was ready`)));
  });
}

async function waitForHttp(url: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    const ok = await fetch(url).then(
      (response) => response.ok,
      () => false,
    );
    if (ok) return;
    await new Promise((wake) => setTimeout(wake, 100));
  }
  throw new Error(`${url} did not answer within 30 s`);
}

async function startArena(): Promise<{ arena: Arena; stop(): Promise<void> }> {
  const home = mkdtempSync(join(tmpdir(), "arena0-e2e-"));
  const env = { ...process.env, ARENA0_HOME: home };
  const background: ChildProcess[] = [];
  const devPort = EMBEDDED ? null : await freePort();
  const devOrigin = devPort === null ? null : `http://127.0.0.1:${devPort}`;
  const ui = spawn(
    BIN,
    [
      "--json",
      "ui",
      "--no-open",
      "--port",
      "0",
      ...(devOrigin === null ? [] : ["--dev-origin", devOrigin]),
    ],
    { env, stdio: ["ignore", "pipe", "inherit"] },
  );
  const uiExited = new Promise<void>((done) => ui.once("exit", () => done()));
  const { url: gatewayUrl } = await firstJsonLine(ui, "arena0 ui");

  let url = gatewayUrl;
  if (devOrigin !== null) {
    const gateway = new URL(gatewayUrl);
    const vite = spawn("pnpm", ["vite", "--port", String(devPort), "--strictPort"], {
      cwd: UI_DIR,
      env: { ...process.env, ARENA0_GATEWAY: gateway.host },
      stdio: "ignore",
    });
    background.push(vite);
    await waitForHttp(`${devOrigin}/`);
    url = `${devOrigin}/${gateway.hash}`;
  }

  const stop = async () => {
    for (const child of background) child.kill("SIGINT");
    // SIGINT is the gateway's Ctrl-C: it closes sockets and stops the daemon it started.
    ui.kill("SIGINT");
    await uiExited;
    rmSync(home, { recursive: true, force: true });
  };

  return {
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
  };
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
