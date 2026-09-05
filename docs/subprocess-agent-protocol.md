# Subprocess agent protocol

Arena0 can bind a local executable to one Host in a coordinated run:

```console
arena0 run prisoner-dilemma \
  --agent host-01=./examples/agents/tit_for_tat.py \
  --agent host-02=./examples/agents/grim.py \
  --replay --no-tui
```

The executable path is invoked directly with the current user's privileges.
Arena0 does not invoke a shell, split arguments, or interpolate protocol data
into the path. One process belongs to one Host for the duration of one run.

## Exchange

Arena0 writes one UTF-8 JSON object followed by a newline for each callout:

```json
{"name":"Choose","prompt":"Choose to cooperate or defect","context":{"round":1},"answer_schema":{"type":"string","enum":["Cooperate","Defect"]}}
```

The executable writes exactly one UTF-8 JSON value followed by a newline:

```json
"Cooperate"
```

Write object and array answers directly. Do not wrap an answer in an envelope.
The Host validates the value against `answer_schema`; the CLI does not convert
or reinterpret it.

The protocol is half-duplex. Read one callout, write and flush one answer, then
wait for the next callout. Standard output is reserved for answers. Write
diagnostics to standard error.

## Bounds and failure behavior

Input and output lines are limited to 64 KiB. An answer must arrive within 30
seconds. Malformed JSON, invalid UTF-8, extra output, timeout, a broken pipe, or
an unexpected process exit fails the run.

Arena0 drains standard error concurrently and retains only a bounded tail for
failure diagnostics. On completion it closes the child's standard input and
reaps the process. On failure or cancellation it kills and reaps the process
before returning. Host-local execution IDs remain available for inspection.

There is no handshake, version envelope, terminal message, command-string
grammar, provider interface, or plugin discovery.

See [Run local agents](run-local-agents.md) for driver combinations, the run
screen, exact local Wasm admission, and receipt verification.
