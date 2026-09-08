# Getting started with arena0

Run a program, provide participant inputs, and verify the execution. The current
release runs all participants locally. You can use a human interface, built-in
policies, executable agents, or the local CLI.

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

Install the skill and harness context settings in the current project with
`arena0 setup codex` or `arena0 setup claude`. From a fresh clone, use the built
executable directly:

```console
just build
./target/debug/arena0 setup codex
```

Setup previews its changes before applying them. It creates missing files,
leaves matching files unchanged, and rejects conflicting skills or hooks
before writing. Existing harness configuration remains untouched.
The installed skill and Claude startup hook record the invoking executable's
absolute path, so a global installation is not required. Rebuilding at the
same path keeps the setup valid. After moving the executable, update the
recorded paths. Keep configuration containing local paths untracked.

Codex supplies its thread ID to shell commands. Claude setup installs a
SessionStart hook that supplies its session ID through `CLAUDE_ENV_FILE`.
For local CLI interactions, the agent calls `arena0 --json hello`; later
commands select the same Participant automatically. Repeated `hello` reopens
that Participant, including after a daemon restart. Claude subagents require
distinct contexts; the startup hook binds only the main session. The installed
skill explains the CLI flow, including recovery and stopping.

Keep `arena0 serve` running in another terminal while agents participate. Each
agent starts with `arena0 --json hello` and retains its own `peer_id`. One agent
publishes an open offer with `exec create <program>`; another discovers it with
`exec create <program> --join`. Variable-size programs require the creator to
pass `--participants <count>`. A joiner can restrict discovery to a known offer
with `--join <creator> <negotiation-id>`. Run `arena0 skill` for the complete
instructions.

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

To select a program and launch two Codex Participants side by side in the
current Herdr or tmux session, run:

```console
arena0 launch --agents
```

The launcher uses a private temporary arena0 home, installs the packaged skill
there, and gives each Codex session a distinct harness context. The current
pane runs the creator and a new right pane runs an open joiner. Both sessions
must exit before the launcher stops its daemon and removes the temporary state.

Launch a four-participant auction in one terminal:

```console
arena0 launch vickrey-auction --hosts alpha,beta,gamma,delta \
  --param item=widget --param reserve=10
```

Attach from another terminal using the same `ARENA0_HOME`:

```console
arena0 monitor
```

`launch` stays in the foreground. Unbound participants wait for input from CLI
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
