import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { assertReply, createClient, DaemonError, OUTCOME_UNKNOWN } from "../dist/index.js";

test("client uses the real daemon HTTP API", { timeout: 120_000 }, async (t) => {
  const home = await mkdtemp(join(tmpdir(), "arena0-client-"));
  const env = { ...process.env, ARENA0_HOME: home, RUST_LOG: "info", NO_COLOR: "1" };
  delete env.ARENA0_UI_DIR;
  delete env.ARENA0_SOCKET;
  delete env.ARENA0_CONTEXT;
  const binary = fileURLToPath(new URL("../../../target/debug/arena0d", import.meta.url));
  const daemon = spawn(binary, [], { env, stdio: ["ignore", "pipe", "pipe"] });
  const stopped = once(daemon, "exit");
  let stdout = "";
  let stderr = "";
  daemon.stdout.setEncoding("utf8");
  daemon.stdout.on("data", (chunk) => { stdout += chunk; });
  daemon.stderr.setEncoding("utf8");
  daemon.stderr.on("data", (chunk) => { stderr += chunk; });
  t.after(async () => {
    if (daemon.exitCode === null && daemon.signalCode === null) daemon.kill("SIGTERM");
    await stopped;
    // Retain the daemon trace outside its home, which can contain private keys.
    const log = `${home}.daemon.log`;
    await writeFile(log, `STDOUT\n${stdout}\nSTDERR\n${stderr}`);
    t.diagnostic(`daemon trace: ${log}; executable: ${binary}`);
    await rm(home, { recursive: true, force: true });
  });
  const baseUrl = await new Promise((resolve, reject) => {
    daemon.once("error", reject);
    daemon.once("exit", (code, signal) => reject(new Error(`daemon exited: ${code ?? signal}\n${stdout}\n${stderr}`)));
    daemon.stdout.on("data", () => {
      const line = stdout.split("\n").find((line) => line.includes("arena0d HTTP listening"));
      const url = line?.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
      if (url) resolve(url);
    });
  });
  const client = createClient(`${baseUrl}/`);
  assert.equal(client.baseUrl, baseUrl);

  await t.test("daemon.info replies belong to the configured daemon", async () => {
    const reply = await client.rpc({ method: "daemon.info" });
    assertReply(reply, "DaemonInfo");
    assert.equal(reply.DaemonInfo.http_url, baseUrl);
    assert.throws(() => assertReply(reply, "Uploaded"), /expected a Uploaded reply/);
  });

  await t.test("missing Hosts preserve the daemon error code", async () => {
    await assert.rejects(client.hostCall("missing-client-test-host", { method: "host.info" }),
      (error) => error instanceof DaemonError && error.code === "NotFound");
  });

  await t.test("octet-stream uploads return their handle and byte length", async () => {
    const uploaded = await client.upload(new Blob([new Uint8Array([0, 1, 127, 255])]), "application/octet-stream");
    assert.equal(uploaded.length, 4);
    assert.equal(typeof uploaded.upload, "string");
    assert.ok(uploaded.upload.length > 0);
    assert.equal(client.blobUrl("host /", "abc"), `${baseUrl}/hosts/host%20%2F/blobs/abc`);
  });

  await t.test("events deliver both default Host starts and MCP tool activity", { timeout: 10_000 }, async (t) => {
    const hosts = new Set();
    let resolveHosts;
    let resolveStarted;
    let resolveFinished;
    const hostFrames = new Promise((resolve) => { resolveHosts = resolve; });
    const startedFrame = new Promise((resolve) => { resolveStarted = resolve; });
    const finishedFrame = new Promise((resolve) => { resolveFinished = resolve; });
    const stream = client.events({
      host(frame) {
        if (frame.kind === "host.started") {
          hosts.add(frame.host.id);
          if (hosts.size === 2) resolveHosts();
        }
      },
      activity(frame) {
        if (frame.kind === "started" && frame.data.tool === "hello") resolveStarted(frame);
        if (frame.kind === "finished") resolveFinished(frame);
      },
    });
    t.after(() => stream.close());
    await once(stream, "open");
    await hostFrames;
    assert.deepEqual([...hosts].sort(), ["host-01", "host-02"]);
    // HTTP RPC has no MCP activity. Drive the real MCP tool path after the
    // Host snapshots establish that the multiplexed stream is subscribed.
    const response = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: {
        Accept: "application/json, text/event-stream",
        "Content-Type": "application/json",
        "MCP-Protocol-Version": "2025-03-26",
      },
      body: JSON.stringify({
        jsonrpc: "2.0", id: 1, method: "tools/call",
        params: { name: "hello", arguments: {} },
      }),
    });
    assert.equal(response.status, 200);
    assert.equal((await response.json()).result.isError, true);
    const started = await startedFrame;
    const finished = await finishedFrame;
    assert.equal(finished.data.call_id, started.data.call_id);
    assert.equal(finished.data.result.kind, "tool_error");
  });

  await t.test("unreachable origins report an unknown outcome", async () => {
    await assert.rejects(createClient("http://127.0.0.1:1").rpc({ method: "daemon.info" }),
      (error) => error instanceof DaemonError && error.code === "transport" && error.message === OUTCOME_UNKNOWN);
  });
});
