# Run local agents

Start the persistent service in one terminal:

```console
arena0 serve
```

The default service owns two independent logical Hosts, `host-01` and
`host-02`. Each has its own identity, store, socket, execution, and producer
receipt, even though both run on one machine.

## Play against a built-in policy

Run Prisoner's Dilemma as the `host-01` Host:

```console
arena0 run prisoner-dilemma \
  --human host-01 \
  --builtin host-02=sample \
  --replay
```

On a terminal, this opens the execution observatory. Its header reports
`LOCAL`, the number of Hosts, and `1 machine`. The lifecycle line starts in
negotiation with no Session, then shows the activated session, public step, and
agreement progress.

The persistent tab bar has six views. Overview stacks Program, Public Trace,
Negotiation, WASM, and System Events with collapsed borders. Program uses the
height required by its current output, while Public Trace receives most of the
remaining flexible space. Tab and Shift-Tab move focus inside the current
view. Enter expands an Overview pane into its matching detail view, and Escape
returns to Overview. Press `1` through `6` to open a view directly.

Overview compares every Host's lifecycle, step, pending callout, receipt, and
bounded private-record count. Negotiation shows the execution identities,
negotiation and activation evidence, participant tickets, and Session state.
Program keeps separately bounded Host-qualified view history. Press `a` to
show all Hosts, `h` or Shift-H to select one Host, and `c` to compare two
Hosts. Left and Right browse exact program steps. The remaining views apply the
same Host scope to the aligned public trace, public and private Wasm handlers,
redacted Host events, and receipt replay.
In the Wasm detail, `<` and `>` load older and newer bounded pages of the
selected Host's redacted private-handler summaries.

When a human-controlled Host receives a callout, a composer opens at the bottom. It
expands to show every applicable prompt, identity, option, context, validation,
and editor row. The content wraps and the workspace yields the required rows.
Concurrent Host requests retain independent `ratatui-textarea` drafts; use `[` and
`]` to move between them. Enter submits the JSON value to that request only.
Escape leaves the composer with the draft intact, `?` opens help, and Ctrl-C
stops the run. If a request is taller than the terminal, input-only mode pins
the editor; Ctrl-PageUp and Ctrl-PageDown scroll its metadata. Outside the
composer, `q` also stops the run. Use
`--no-tui` for inline prompts. Set `NO_COLOR` to disable color. The minimum
screen size is 48 columns by 23 rows.

## Run executable agents

The repository includes two directly executable Python examples:

```console
arena0 run prisoner-dilemma \
  --agent host-01=./examples/agents/tit_for_tat.py \
  --agent host-02=./examples/agents/grim.py \
  --replay \
  --no-tui
```

Each path is one executable, not a shell command. One process belongs to one
Host for the run. See the [subprocess agent protocol](subprocess-agent-protocol.md)
for the JSONL exchange and its bounds.

## Run without interaction

Use a built-in policy for every Host when stdout must be machine-readable:

```console
arena0 --json run chess \
  --builtin host-01=sample \
  --builtin host-02=sample \
  --replay
```

`sample` contains small policies for the bundled supported programs.
`first-allowed` handles a closed enum by selecting its first allowed value.
Neither policy discovers tools, loads plugins, or acts as a general agent.
`--human` cannot be combined with `--json`.
In the full-screen TUI, repeat `--human HOST` to answer several Hosts through
one shared input queue. Each answer remains bound to that Host's exact pending
callout. The bare `arena0` workspace offers the same choice as **One Host** or
**All Hosts**; changing the run screen's Host view never changes who supplies
answers.

## Run local Wasm

Pass a local Wasm path instead of a catalog name:

```console
arena0 run ./target/wasm32-unknown-unknown/release/my_program.wasm \
  --human host-01 \
  --builtin host-02=sample \
  --replay
```

The coordinator imports the exact bytes into every selected local Host and
then uses exact local admission. It does not discover peers, transfer programs
to remote machines, or let one Host's catalog stand in for another's.

## Understand completion

Each Host has an `ExecId` before and after activation. Negotiation is
pre-session work identified by a `NegotiationId`. A `SessionHash` exists only
after every participant commits the activation.

The command drives all Hosts concurrently, requires them to report the same
session and shared terminal facts, and verifies every producer receipt. The
default is light verification; `--replay` asks each Host to perform full Wasm
replay. A failure or cancellation stops and reaps subprocess agents, while the
Host-local execution records remain available through `arena0 exec` commands.
