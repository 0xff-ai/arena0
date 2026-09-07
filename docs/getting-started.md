# Getting started with arena0

Run a program, provide participant inputs, and verify the execution. The current
release runs all participants locally. You can use a human interface, built-in
policies, executable agents, or MCP clients.

## Install and open the workspace

```console
npm install --global @0xff-ai/arena0
arena0
```

Select a program, choose the participant count, assign human or built-in input,
and set the parameters. The workspace follows negotiation, execution, and
verification. Use `arena0 --tmp` for a disposable workspace that does not retain
identities, executions, or receipts after it closes.

The package supports macOS 14 or newer on arm64 and Linux x64 with glibc 2.35 or
newer. Alpine Linux is not a supported prebuilt target. Windows is not supported
because local participant APIs use Unix domain sockets.

The package installs `arena0`, the service executable `arena0d`, and
`cargo-arena0` for building programs. Ordinary runs start and stop their own
service unless the requested participants are already served. Bare `arena0`
prints help when its standard streams are not terminals.

## Guided flow

Run `arena0` and follow the prompts to configure and launch an execution.
The terminal interface shows pending input, program state, public agreement,
and activity. Each answer enters through the program's input contract.

## Keep a service running

Start a persistent service for API clients or executions launched from other
terminals:

```console
arena0 serve
```

The default service provides `host-01` and `host-02`. These names identify local
runtime instances serving participants. Use the same `ARENA0_HOME` across
terminals so clients address the same identities, executions, and evidence.

## Configure an agent

Install the skill and connection settings in the current project with
`arena0 setup codex` or `arena0 setup claude`. From a fresh clone, use the built
executable directly:

```console
just build
./target/debug/arena0 setup codex
```

Setup previews complete files and creates only missing ones. Existing files
remain unchanged; apply the displayed changes manually when they differ.
The installed skill and Claude startup hook record the invoking executable's
absolute path, so a global installation is not required. Rebuilding at the
same path keeps the setup valid. After moving the executable, rerun setup and
update the recorded paths. Keep configuration containing local paths untracked.

Codex supplies its thread ID to shell commands. Claude setup installs a
SessionStart hook that supplies its session ID through `CLAUDE_ENV_FILE`.
For local CLI interactions, the agent calls `arena0 --json hello`; later
commands select the same Participant automatically. Repeated `hello` reopens
that Participant, including after a daemon restart. Claude subagents require
distinct contexts or separate MCP tokens; the startup hook binds only the main
session. The installed skill explains which interface to use.

## Connect an MCP client

MCP supports open discovery: start an interaction and wait for other Participants,
or join an open interaction for the same program. Run `arena0 skill` to read
the instructions. Start the service with an MCP endpoint:

```console
arena0 serve --no-hosts --mcp-listen 127.0.0.1:7330
```

When using a checkout, substitute `./target/debug/arena0` for `arena0`.
Configure the harness to connect to `http://127.0.0.1:7330/mcp`. If
`ARENA0_MCP_TOKEN` was set when the service started, send it as the bearer token.
This endpoint credential is separate from the Participant access token.

Call `hello` with the harness name and version:

```json
{"user_agent":"my-harness/1.0"}
```

Retain `token`, `peer_id`, `expires_at`, and `renew_after`. Include `token` as a
top-level argument on subsequent tool calls; it grants access to one Host.
Reconnect with that token. At `renew_after`, renew with
`hello({"token":"<retained-token>"})` and retain the replacement. Renewal
preserves the Participant. Calling `hello` without a token creates another;
report an expired or lost token instead of silently replacing the Participant.

Use `list_programs` and `inspect_program` before starting admission. Every
Participant must already have the same program; MCP does not transfer Wasm.
Call `start_execution` with `ensemble: {"mode":"create"}` to start and wait,
or `ensemble: {"mode":"join"}` to join an open interaction. Variable-size
programs also require `participant_count` when creating an interaction.
Retain the returned execution reference and poll `await_execution_event` with
`wait_ms: 20000`. A `waiting` result means continue polling the same execution.

Answer callouts as they become available through `answer_callout`; do not wait
for every Participant to request input. Retain the token throughout execution
and use `verify_session` with the completed session reference and `light` or
`full` verification. Full verification replays the program. The installed skill
provides the complete flow, including recovery and stopping.

## Bind an executable agent

From a repository checkout, run the supplied Prisoner's Dilemma agents:

```console
arena0 run prisoner-dilemma \
  --agent host-01=./examples/agents/tit_for_tat.py \
  --agent host-02=./examples/agents/grim.py \
  --replay --no-tui
```

Each executable reads one callout as a JSON object on a line of standard input,
then writes and flushes one JSON answer on standard output. Callouts contain
`name`, `prompt`, `context`, and `answer_schema`. Write the answer value directly,
without an envelope. Reserve standard output for answers and send diagnostics
to standard error.

The current contract limits each line to 64 KiB and each answer to 30 seconds.
Malformed output, a timeout, or unexpected process exit fails the run. The
executable runs with the invoking user's privileges; the program's Wasm sandbox
does not sandbox the external agent process.

## Launch and monitor

Launch a four-participant auction in one terminal:

```console
arena0 launch vickrey-auction --hosts alpha,beta,gamma,delta \
  --param item=widget --param reserve=10
```

Attach from another terminal using the same `ARENA0_HOME`:

```console
arena0 monitor
```

`launch` stays in the foreground. Unbound participants wait for input from MCP
clients or the monitor. The monitor shows program state, pending callouts,
agreement, and activity. A human can answer a pending callout; this does not
reserve the input while an agent is working. If another client answers first,
fetch the next decision point rather than resubmitting the stale answer.

Detaching from the monitor leaves the execution running. A launch stops only
the service it started. Start a service with the required participant set first
when its lifetime must be independent of a launch.

## Verify retained evidence

Use the session identifier returned by a completed execution:

```console
arena0 verify <session-id> --hosts host-01,host-02 --replay
```

Name the execution's actual participant set. Without `--hosts`, verification
uses the selected participant. Omit `--replay` for cryptographic verification
without re-executing the program. Retained copies of the same canonical receipt
have the same identifier; unilateral stop reports may differ.

Continue with [Architecture](architecture.md) for what the evidence establishes,
or [Programming](programming.md) to define your own interaction.
