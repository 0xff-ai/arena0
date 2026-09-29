// Live end-to-end harness: one real daemon per Playwright worker.
//
// `arena0 ui` runs against a fresh ARENA0_HOME with its default two Hosts and
// the bundled programs. Tests seed state through the real CLI (`arena0 run`,
// `arena0 launch`) against that same home, then drive the page. Nothing is
// faked. The binary comes from ARENA0_BIN, else ../target/debug/arena0.
//
// The page comes from one of two places:
// - default: a Vite dev server per worker, proxying HTTP to that worker's
//   daemon, so suites test the current source without rebuilding the binary;
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
const FIRST = resolve(UI_DIR, "../examples/agents/first_allowed.py");
const CHESS = resolve(UI_DIR, "e2e/agents/first_legal_move.py");
const EMBEDDED = process.env.ARENA0_E2E_EMBEDDED === "1";
/** crates/arena0-daemon/src/exec_manager.rs `NEGOTIATION_TIMEOUT`. */
const NEGOTIATION_TIMEOUT_MS = 30_000;

export interface Arena {
  /** The page URL served by the daemon or the dev server. */
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
  // A worker owns a fresh two-Host daemon. Do not inherit the coding agent's
  // selected Host namespace into CLI calls that intentionally use the default.
  const env: NodeJS.ProcessEnv = { ...process.env, ARENA0_HOME: home };
  delete env.ARENA0_CONTEXT;
  delete env.CODEX_THREAD_ID;
  delete env.ARENA0_SOCKET;
  const background: ChildProcess[] = [];
  const devPort = EMBEDDED ? null : await freePort();
  const devOrigin = devPort === null ? null : `http://127.0.0.1:${devPort}`;
  const ui = spawn(BIN, ["--json", "ui", "--no-open"], {
    env,
    stdio: ["ignore", "pipe", "inherit"],
  });
  const uiExited = new Promise<void>((done) => ui.once("exit", () => done()));
  const { url: daemonUrl } = await firstJsonLine(ui, "arena0 ui");

  let url = daemonUrl;
  if (devOrigin !== null) {
    const vite = spawn("pnpm", ["vite", "--port", String(devPort), "--strictPort"], {
      cwd: UI_DIR,
      env: { ...process.env, ARENA0_DAEMON_URL: daemonUrl.replace(/\/$/, "") },
      stdio: "ignore",
    });
    background.push(vite);
    await waitForHttp(`${devOrigin}/`);
    url = `${devOrigin}/`;
  }

  const stop = async () => {
    for (const child of background) child.kill("SIGINT");
    // The UI command stops the daemon it started when it receives Ctrl-C.
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

export const test = base.extend<{ daemonState: void }, { arena: Arena; suite: string }>({
  // Each spec file sets its own `suite`. Workers are keyed by their worker
  // options, so files never share a worker, and so never share a daemon: a
  // suite sees only the sessions it seeded.
  suite: ["", { scope: "worker", option: true }],
  arena: [
    async ({ suite }, use) => {
      if (suite === "")
        throw new Error('call test.use({ suite: "<name>" }) in every live spec file');
      const { arena, stop } = await startArena();
      await use(arena);
      await stop();
    },
    { scope: "worker" },
  ],
  // On failure, attach what the daemon holds (every Host's executions, with
  // lifecycle and terminal reasons) so a failed run explains itself instead
  // of being rerun.
  daemonState: [
    async ({ arena }, use, testInfo) => {
      await use();
      if (testInfo.status === testInfo.expectedStatus) return;
      const status = JSON.parse(await arena.cli(["--json", "status"])) as {
        hosts: { host: { id: string } }[];
      };
      for (const { host } of status.hosts) {
        const executions = await arena
          .cli(["--json", "--host", host.id, "exec", "list"])
          .catch((error: unknown) => String(error));
        const path = testInfo.outputPath(`${host.id}-executions.json`);
        writeFileSync(path, executions);
        await testInfo.attach(`${host.id}-executions.json`, {
          path,
          contentType: "application/json",
        });
      }
    },
    { auto: true },
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

/** Open the workspace at `path` (which may carry a `?search`) and wait until the snapshot is live. */
export async function openApp(page: Page, arena: Arena, path = "/"): Promise<void> {
  const base = new URL(arena.url);
  const url = new URL(path, base);
  await page.goto(url.toString());
  await expect(page.getByTestId("connection")).toHaveText(/live/, { timeout: 30_000 });
}

/** Seeds through the real CLI. Program names are the bundled catalog names. */
export const seed = {
  /** Run a program to completion with executable agents on both Hosts. */
  async completed(arena: Arena, program: string): Promise<void> {
    await arena.cli([
      "--json",
      "run",
      program,
      "--agent",
      `host-01=${FIRST}`,
      "--agent",
      `host-02=${FIRST}`,
    ]);
  },
  /**
   * Start a session where host-01 plays the program's bundled example policy
   * and host-02 is left to the user, and resolve once host-02 has an open
   * callout for the page to answer. Chess uses a legal-move agent because
   * its answers are free-form; the other programs use their first enum value.
   */
  async awaitingYou(arena: Arena, program: string): Promise<{ execId: string }> {
    arena.spawn([
      "--json",
      "launch",
      program,
      "--agent",
      `host-01=${program === "chess" ? CHESS : FIRST}`,
    ]);
    // Negotiation alone may take up to the daemon's 30 s negotiation timeout.
    const deadline = Date.now() + NEGOTIATION_TIMEOUT_MS + 15_000;
    while (Date.now() < deadline) {
      const list = JSON.parse(await arena.cli(["--json", "--host", "host-02", "exec", "list"])) as {
        executions: {
          exec_id: string;
          state: { exec_state: string; session?: { pending_callout: unknown } };
        }[];
      };
      const waiting = list.executions.find(
        (e) => e.state.exec_state === "Active" && e.state.session?.pending_callout != null,
      );
      if (waiting !== undefined) return { execId: waiting.exec_id };
      await new Promise((wake) => setTimeout(wake, 250));
    }
    throw new Error(`host-02 has no open ${program} callout after the negotiation deadline`);
  },
};

/** Answer each callout through exec next / exec submit with its first enum
 * value until the daemon reports Completed or Failed. */
export async function playFirstAllowed(arena: Arena, host: string, execId: string): Promise<void> {
  for (;;) {
    const next: {
      Callout?: { pending_id: string; schema: { enum: unknown[] } };
      Completed?: unknown;
      Failed?: unknown;
    } = JSON.parse(await arena.cli(["--json", "--host", host, "exec", "next", execId]));
    if ("Completed" in next || "Failed" in next) return;
    const callout = next.Callout!;
    await arena.cli([
      "--json",
      "--host",
      host,
      "exec",
      "submit",
      execId,
      "--pending-id",
      callout.pending_id,
      "--answer",
      JSON.stringify(callout.schema.enum[0]),
    ]);
  }
}

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
      // Theme changes fade colours; capture the settled frame. Infinite
      // animations (pulsing status dots) never settle and are ignored.
      await page.waitForFunction(() =>
        document
          .getAnimations()
          .every(
            (animation) =>
              animation.playState !== "running" ||
              animation.effect?.getTiming().iterations === Number.POSITIVE_INFINITY,
          ),
      );
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
