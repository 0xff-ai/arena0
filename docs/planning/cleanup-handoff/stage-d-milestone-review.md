# Stage D milestone review — branch `raulk/fix-dual-arch` at `ae94a88` vs base `5caeea9`

Reviewer: delegated read-only review (no source edits, no suite reruns, no agents spawned).
Scope: whole branch `5caeea9e4a4fb4ea6e9edbfae72e82ded881c891..ae94a88`
(192 files, +15,628 / −27,422), with focus per the review contract.
Checkpoint `ae94a88` == D3b worktree HEAD plus the untracked macro test.
Base is local `main`.

Evidence reused without rerun: `.bb/d3b-gate.log` (`exit=0`; 727 host tests,
22 program tests, SDK/primitives doctests incl. 6 compile-fail cases).
Read-only diagnostics only: `git diff`/`show`/`log -S`, `rg`, and file reads.
`git diff HEAD --check`-equivalent cleanliness was verified by the parent gate;
I ran no builds or test suites. Limits: I did not re-derive the full
deleted-test universe beyond the D3b report plus `.bb/d3b-dropped.txt` and the
`ae94a88` diff; timing/load behavior (daemon MCP flakes) was not reinvestigated.

## Verdict

No merge-blocking shape deviation. The review contract's final D3b surface is
met exactly as amended by the parent (`Run::query` removed, chess helpers
accepted, data-macro and draw-rule tests restored). D2b honors its
byte-compatibility constraint; the version bumps on the branch pre-date D2b.
Compile-once and the stable Wasmtime cache hold on the production and Arena
paths. Store and SDK ownership match the decided model.

Three actionable findings below (two medium coverage gaps introduced by D3b,
one low latent race in new Arena code), plus two follow-up dispositions with
dependency evidence and one recorded non-finding parents often ask about.
None requires a new runtime, a new abstraction, or reviving the harness.

---

## Finding 1 (Medium) — terminal projection *content* lost its only assertions

Trigger: D3b deleted every test that rendered a terminal board and asserted on
its text, mapping them to "dropped: native harness only":

- `programs/rock-paper-scissors` `view_renders_revealed_hands_and_scores`
  (revealed hands + scores after the match),
- chess terminal-board texts (mate board; the retained Arena test covers only
  the mid-game board: `chess_bilateral.rs` asserts `Last move: e5`, kings,
  mono-no-SGR *before* mate),
- contract-net `P1 @ 7` / accepted terminal texts, vickrey `Sold to P`
  terminal text (same deletion pattern).

Mechanism: retained Arena runs assert callout contexts, rejection reasons,
*mid-game* views, outcomes, and verified receipts — none of which observes the
terminal `View` slots. The parent-restored
`programs/chess/src/lib.rs:727 draw_rules_classify_terminal_boards` pins the
*rule* (`Status::compute`) but not the *rendering* (`ProgramView::view` on a
terminal `Shared`) nor the `Status → Outcome` mapping for draws
(`lib.rs:213 outcome` maps all three draw statuses; only the checkmate path is
exercised end-to-end by the scholars-mate Arena run).

Observable consequence: a terminal-rendering regression (wrong winner text,
missing slot, private-state leak into a terminal slot) passes the full gate:
727 host + 22 program tests stay green.

Why the harness-only label is wrong here: rendering a `View` from a
constructed terminal `Shared` never needed the harness. Contract §4 explicitly
keeps "native `#[test]`s … (`Program::outcome` / `ProgramView::view` /
`writer` on a constructed `Shared`)", and the integrated
`examples/minimal-program` test proves the pattern compiles and passes
(`view_uses_all_four_slots_and_plain_text` builds `Shared` + `Ensemble` +
`Viewport` directly).

Bounded remedy (owner: parent; no harness, no runtime change): add native pure
view tests on constructed terminal states — RPS revealed-hands/scores content
and chess mate-board header/status (2–4 tests in `programs/*`, following the
minimal-program pattern). Optionally pin chess draw `Status → Outcome`
mapping through `Program::outcome` on the same constructed states.
Proof: the new tests fail if terminal slot text regresses; `just test`
stays the gate. This is an introduced gap (D3b dropped the assertions), not a
pre-existing limitation.

## Finding 2 (Medium) — post-terminal input boundary is unpinned on any Host

Trigger: chess `move_after_game_over_rejected` (input after checkmate is
rejected, no broadcast) was dropped; no Arena or daemon test submits input
after terminal on any program.

Mechanism: `crates/arena0-node/src/execution/guest.rs:119 submit_input`
returns `CalloutNotPending` when `state.callout()` is `None`, which is the
specified terminal behavior ("terminal status has no open callout",
`docs/protocol-architecture.md:469`). But the live actor retires at `Ended`
(`actor.rs`: `if self.end_run_finished() { return; }`), after which
`cmd_tx.send` fails and `Run`-level submit panics with "execution gone"
instead of returning `CalloutNotPending`. So the observable post-terminal
behavior is timing-dependent: clean rejection while `Ending`, send-failure
panic once `Ended`. No test pins either, and the daemon `exec.submit` path
after retirement was not traced in this review.

Observable consequence: an agent (or retry loop) that answers a completed
session gets an unspecified failure mode; a future change that turns the
panic into a hang or a silent accept has no test to catch it.

Bounded remedy (owner: parent; needs a disposition choice, not just a test):
either (a) a daemon-level test that drives RPS/chess to completion via
`exec.*` then submits and asserts one stable error, or (b) an explicit
recorded decision that post-terminal submit is unpinned (callers must observe
`Completed` first). Do not pin it through `Run::view`-style live-actor calls
— see Finding 4, the same retirement race applies. Pre-existing limitation
(the actor always retired; D3b did not introduce it), but the *assertion* was
introduced-loss: the old harness test was the only pin on the rule.

## Finding 3 (Low) — `Run::callout` reads newest, `Expect::submit` answers oldest

File/line: `crates/arena0-tests/src/arena.rs:616` (`handle.events.iter().rev().find_map`)
vs `:1003` (`participant.events.iter().find_map`).

Trigger: two `CalloutRequested` events with distinct `pending_id`s coexisting
in one participant's buffer — e.g. a restart re-announcement (announced-callout
state resets) followed by a genuinely new callout before `respond_bytes`
retains, or back-to-back callouts answered out of order.

Mechanism: `callout()` returns the newest context while `submit()` answers
the oldest `pending_id`. On mismatch the actor returns `CalloutNotPending`
(`guest.rs:128`) and `respond_bytes` panics with "input rejected" on a
perfectly valid answer; `respond_rejected` panics with "expected an input
rejection". The two functions also disagree with the contract text only in
spirit (§3 says "the same callout" / newest); behavior matches today because
`announce_callout` dedups by id and `respond_bytes` retains *all*
`CalloutRequested` events, so at most one distinct id is normally present —
which is why 727 tests pass.

Observable consequence today: none (benign). Risk: a flaky, order-dependent
Arena panic under restart/retry interleavings with a misleading message.

Bounded remedy (owner: parent): make `submit` select the newest
`CalloutRequested` (`rev().find_map`, mirroring `callout`), or assert at most
one distinct `pending_id` in the buffer before submitting. One-line change;
proof is the existing Arena suite (no input change → no full rerun needed
beyond the affected `arena0-tests` package). Introduced shape (new D3b code),
pre-existing absence of failure.

## Finding 4 (Uncertain → resolved, no action): terminal views and the architecture promise

Question asked: actor "stops before a final view can be requested" — regression
or limitation, and does docs/architecture promise terminal viewing?

Resolved by reading code, not by prescribing a runtime:

- `docs/api/json-rpc.md:88` promises `exec.view` "renders … during execution
  and after termination. Terminal views use the saved shared state and remain
  available after the live execution driver exits." The production daemon path
  implements exactly this: `crates/arena0-daemon/src/server.rs:3412 view()`
  serves terminal lifecycle from the durable `shared_state` + catalog program
  through the shared engine (`spawn_blocking`), never touching the actor.
- That promise is tested: `crates/arena0-tests/tests/daemon_api.rs:567
  exec_view_distinguishes_negotiating_active_terminal_and_missing_executions`
  asserts non-empty terminal `ExecView` with `step > 0` on both hosts after
  `Completed`.
- `docs/protocol-architecture.md` promises "terminal projections" only as a
  JSON boundary class (§9) and "read-only … view … projections create fresh
  bounded Wasm instances over explicit state snapshots" (§9/§10) — consistent
  with the daemon implementation. It also states "The actor retires at
  `Ended`" (§10), so `Run::view` (live `ExecCommand::View`) failing after
  retirement is an *existing Arena-harness limitation*, not a D3b regression
  and not an architecture violation.
- Consequence for Findings 1–2: terminal *content* can be retained as native
  pure tests (Finding 1) and terminal *service availability* is already covered
  at the daemon layer; do not add a stored-projection path to `Run` and do not
  revive any harness. No new runtime is warranted — the evidence cuts against it.

## Follow-up dispositions (no implementation; dependency evidence)

**F-a. Bounded helpers in wire vs program: keep both owners, no migration.**
`arena0-program::bounded` (field codecs: `write_bytes/read_bytes`,
`write_string`, option-tag `0/1`, `write_vec/read_vec`; Borsh-stock encodings)
is imported by every protocol bounded field
(`protocol/src/{effect,event,timer,trace/entry,execution/*}.rs` all
`use arena0_program::bounded`). `arena0-wire` keeps only `pub(crate)`
frame-layer helpers (`wire/src/lib.rs:47 serialize_bounded_bytes`,
`:77 serialize_bounded_vec`, `:110 read_bounded_bytes`; `codec.rs`
`BoundedWriter` + `Codec::check_body_size` enforce the `[u32 len][u16
version][borsh body]` envelope). D2b's design put wire out of scope and wire
depends only on `arena0-crypto` — adding a wire→program dependency to share
~10 lines would cross a deliberate layering boundary for no behavioral gain.
The task-ledger "decide after D3b" item should close as "no change".

**F-b. `resolve_test_cache_dir` visibility: narrow to `pub(crate)`, whenever
touched.** Actual callers: `arena0-test-engine/src/lib.rs:31` (in-crate) and
its own unit test; zero external callers (`rg` over `crates/` shows only those
plus doc mentions). The `arena0-sandbox/src/test_support.rs:34` private copy
exists to avoid a dependency cycle (documented in its header) and is likewise
crate-local. The function is genuinely internal-only, so the ledger note is
accurate — but the crate is `publish = false` test support, so exposure is
nil. Bounded remedy: one-word change `pub fn` → `pub(crate) fn` in
`test-engine/src/lib.rs:57` with a focused build; no sync test needed (the
resolver already has `resolver_anchors_everything_at_the_workspace_root`).

## Verified conformance (no findings; stating what was checked)

- **Review-contract D3b surface.** `arena.rs` adds exactly `ObservedCallout`
  (+267), `Run::callout` (+106), `Run::view` (+153), `Expect::respond_rejected`
  (+291), and private `Expect::submit` (+306); `respond_bytes` = submit → panic
  on `Err` → retain-remove; `respond_rejected` returns the reason only on
  `Err(InputRejected)`, panics on `Ok` or other errors. `Run::query` is absent
  (verified: no `fn query` in `arena.rs`, no `.query(` in Arena tests) —
  accepted parent amendment (ledger H004; no program advertises a query).
  `Viewport`/`View` are the protocol types. Unlisted chess `STARTING_FEN` +
  `mono_viewport()` — accepted parent deviation, test-only.
- **D3b deletions/shims/docs.** `testing{,/diagnostics,/fixtures,/harness,/orchestration}.rs`,
  `sdk/tests/fixtures.rs`, `arena0_test.rs` + macro entry, divergence module +
  re-exports are gone; every non-wasm32 shim in `sdk/src/effects.rs` funnels
  to `native_host_unavailable` with one `let _ =` consumption line; `fake_sign`
  and native sinks deleted. The contract grep prints nothing (re-ran
  read-only: zero hits outside `target/`). `docs/programming.md` + `docs/development.md`
  carry the pure/Arena split with the `just build-programs` prerequisite.
  Restored `data_macro.rs` compiles explicit generic contract bounds; chess
  draw-rule test covers all three terminal rules through `Status::compute`.
- **D2b (first independent model review).** Single owner
  `arena0-program::bounded` with stock-Borsh encodings; protocol's private copy
  deleted and callers moved; `CallStatus`/`Option` tags stable (`0/1`,
  unknown tags rejected); golden tests present (`call_status_uses_stable_v1_tags`,
  `flat_event_tags_are_fixed…`, `timer_encodings_keep_bounds…`,
  `dispatch_output_rejects_invalid_reason_combinations_and_bounds`, state
  limit tests). Over-limit prefixes rejected before allocation on every reader;
  writers reject with `InvalidInput`. The "no version constant changes"
  constraint holds for D2b itself: `git log -S` attributes `ABI_VERSION 21→22`,
  `EXECUTION_PROFILE_VERSION 2→3`, receipt `3→5` to earlier commits
  (`9e3a079`, `bf49bcf`), and `git show 92f2fb7` contains no `VERSION` value
  change — the commit message's "byte-identical" claim checks out within its
  scope. Error-text change (`{field} exceeds bound` → `length {len} exceeds
  bound {MAX}`) is the design-mandated message, covered by unit tests.
- **SDK modes (D2e).** One `Ctx<Shared, Local, M>`; only `AgreedMode` and
  `LocalMode` implement `EffectMode` (`context.rs:100,118`) — `ReadMode` has
  no broadcast path, so `CalloutContext` is read-only by construction.
  `shared_mut`/`state_mut`/`mutate_shared` exist only on `Ctx<…, AgreedMode>`;
  `LocalMode` exposes `shared()` read-only plus `sign`. Matches the audit's
  F1 target shape.
- **Sandbox plain args (D2d).** `DispatchCall::new(peer_id, session, event)`
  plus builder flags; `LoadedProgram::{initialize, writer, query, view,
  outcome}` take explicit snapshots, not wrapper structs. Required exports
  unchanged (`validation.rs:15 REQUIRED_FUNC_EXPORTS`, all nine guest entries
  + `arena0_abi_version` global).
- **Store single facts (D2c).** `terminal_proofs.publication` removed in favor
  of the `receipt_id → receipts` FK; `inbox`/`inbox_conflicts`/`outbox` tables
  and indexes deleted, matching architecture §10 ("no inbox or outbox
  tables"); executions gain `end_phase`/`end_unconfirmed`; version-tripwire
  persist with one transaction per transition (`execution.rs:349
  validate_terminal_rows`, `:433 persist_terminal_publication`, `:439
  assemble_receipt` from local durable facts only).
- **Compile-once + stable cache (D3a requirement).** Production:
  `daemon/src/run.rs:45` builds one `WasmtimeEngine::new_persistent` on the
  stable home cache dir and shares it as `Arc` across all Hosts
  (`ensemble.rs:300,332,461,856`); `WasmtimeEngine::load` memoizes by program
  hash in a `moka` cache (`sandbox/src/engine/instance.rs` + `program.rs`
  `load`), so each guest compiles once per engine with Wasmtime persistent
  cache reuse across runs. Arena: one `shared_test_engine()` load per run
  (`arena.rs:228`, `fixtures.rs:193`) through `arena0-test-engine`'s stable
  `wasmtime-cache` dir (workspace-anchored, cwd-independent). Unit tests use
  the same shared engine. Only `cargo-arena0 build` (dev-time finalizer) uses
  ephemeral `WasmtimeEngine::new()` — correct for a build tool. Requirement
  holds on actual paths, not just helpers.
- **Race/deadline behavior (beyond Finding 3).** `callout`/`submit`/terminal
  waits poll at 10 ms with full diagnostics on timeout (events + loaded states
  + progress summary) — legitimate event polling, not sleep-to-green.
  `submit_input` correctly distinguishes `CalloutNotPending` (expected) from
  fatal errors, preserves the continuation on rejection, and surfaces
  `AgreementPending` for retry with the same id. `respond_rejected` leaves the
  callout open by construction (no retain); `respond_bytes` retains all
  `CalloutRequested`. No evidence of weakened assertions, skips, retries, or
  timeout inflation anywhere in the D3b diff. Successful runs are not cited as
  race-freedom proof (Finding 3 is the residual risk).
- **Whole-branch API ownership.** No new duplicate concepts found: D2e
  collapsed the three context types the audit flagged (F1); D2a/D2d/D2c
  removals (dual paths, inbox/outbox, envelope wrappers) match their designs;
  `synthetic.rs`/`common/mod.rs` changes are mechanical adaptations to the
  `StepEvent`/`StepTerminal` model plus a `chess_wasm()` helper. New test-only
  programs (`local-context-forge`, `timer-dispatch-*`) serve post-D3b
  dispatch coverage. One note: error/message enum imports shifted
  (`MessageId` → `StepCommitment`, `TerminalCommitment` removed) as part of the
  decided evidence-model rework — consistent, not duplication.

## Coverage accounting (deleted → retained, product-relevant subset)

| Deleted assertion | Retained equivalent | Disposition |
|---|---|---|
| chess scholars-mate flow, illegal/short-move rejections, mid-game board/kings/mono | `chess_bilateral.rs::chess_bilateral_scholars_mate_runs_and_verifies` (starting-FEN callout, 2 rejection reasons, e4/e5 sync, mid-game view, identical outcomes, verified receipts) | covered |
| chess draw *rules* (stalemate/50-move/material via handler) | `chess::draw_rules_classify_terminal_boards` (pure `Status::compute`) | rule covered; *rendering + Outcome mapping* open → Finding 1 |
| chess post-terminal input rejection | none | open → Finding 2 |
| RPS rounds, Lizard rejection, clinch 2–0, mid-game views | `rock_paper_scissors_bilateral_runs_and_verifies_receipts` | covered except terminal-hands rendering → Finding 1; draw-round path unpinned (minor, same remedy class) |
| PD rounds/scores/history/matrix views, Cheat rejection | `prisoner_dilemma_bilateral_runs_and_verifies` + pure `payoff_matrix_is_canonical` / `outcome_projects_scores_and_winner` | covered |
| contract-net offers/proposal/accept, duplicate-bid rejection, mid-game views | `contract_net_runs_and_every_producer_verifies` | covered |
| cumulative-sum / sequential-count views | trilateral/multiparty runs | covered |
| vickrey bids, non-integer rejection, mid-game state view, verified receipts | `vickrey_auction_runs_and_every_producer_verifies` | covered except pending/no-reveal path (minor; same remedy class as Finding 1 if parent wants it) and terminal `Sold` text → Finding 1 |
| minimal-program converge + view | two native pure tests (`outcome_ranks_choices_by_score`, `view_uses_all_four_slots_and_plain_text`) | covered (with `..Default::default()` phase deviation, accepted — phase irrelevant to both assertions) |
| harness mechanics (replay/coverage/ledger/schedules/divergence/transcripts) | none | legitimately dropped (deleted machinery) |

Counts were not used as coverage proof; every row above was checked against
actual test bodies.

## What I inspected and limits

Read in full or in the relevant ranges: review contract, ledger
(`claude-handoff-ledger.md`), `D3b.md`, task ledger + audit + `design-d2b.md`
in `thr_6b9is45iux/impl/`, `d3b-report.txt`, `d3b-gate.log` tail,
`d3b-dropped.txt` head, `arena.rs` callout/view/submit/wait paths,
`actor.rs` loop + `handle` + terminal-boundary code, `terminal.rs`,
`guest.rs` submit/view/query, `context.rs` modes, `sdk/src/lib.rs` +
`prelude.rs`, `program/src/bounded.rs` + `abi.rs` tests, `wire/src/codec.rs` +
`wire/src/lib.rs` bounded fns, `sandbox` program/instance/test_support,
`test-engine/src/lib.rs`, `daemon/src/run.rs` + `server.rs view()` +
ensemble engine plumbing, store schema diff + `execution.rs` persist path,
`docs/protocol-architecture.md` §§9–11 + `docs/api/json-rpc.md` exec table,
all seven program Arena tests (chess + RPS in full), `data_macro.rs`,
`daemon_api.rs` terminal-view test, version-bump provenance via `git log -S`.
Did not inspect: full `execution/state.rs` + `validation.rs` proofs (relied on
suite evidence), CLI/TUI changes (out of focus), daemon MCP timeout root cause
(recorded pre-existing, load-dependent, unchanged by this review).
