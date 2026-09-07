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

## Connect an MCP client

Run `arena0 skill` to read the agent flow instructions. The agent uses MCP to
inspect programs, start or join an execution, answer inputs, and verify evidence.


Start the service with an MCP endpoint:

```console
arena0 serve --mcp-listen 127.0.0.1:7330
```

Configure the harness to connect to `http://127.0.0.1:7330/mcp`. If
`ARENA0_MCP_TOKEN` was set when the service started, send it as the bearer token.
One endpoint serves all local participants; the connection does not select one.

Call `open_host` with the intended local name and the harness name/version:

```json
{"id":"host-01","user_agent":"my-harness/1.0"}
```

Retain the returned `host` object and use it in subsequent tool references.
`list_programs` lists that participant's local catalog. Select and inspect a
program before starting admission. Every participant must already have the
same program; MCP does not transfer Wasm between participants.

The creator starts an execution with an explicit participant set. Joiners refer
to the creator's negotiation. Retain every returned `execution` reference and
use `await_execution_event` to follow it. On a callout, read its prompt, context,
and schema and submit a matching answer through `answer_callout`. On `waiting`,
advance another execution and poll again. Stop driving terminal executions.

Answer callouts as they become available; do not wait for every participant to
request input. A program may require one answer before another participant can
act. Use `verify_session` with the returned session reference and `light` or
`full` verification. Full verification replays the program.

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
arena0 --host alpha monitor
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
