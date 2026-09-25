# arena0 cleanup — code design (guest-side stage)

Author: integration owner. Workers implement this design; they do not redesign
it. If the code contradicts this design, stop and report the conflict in your
result instead of inventing a different behavior.

Repository: /data/arena0, branch raulk/fix-dual-arch. Baseline commit 79e3e09.

## Global rules (every step)

- Read AGENTS.md and follow it. `docs/protocol-architecture.md` owns protocol
  behavior; step A revises it to this design first.
- No backward compatibility and no migrations. Rewrite formats, Borsh tags,
  schema and ABI in place. In-repo programs (`programs/*`,
  `examples/minimal-program`, `crates/arena0-sdk` doc/examples, test fixtures)
  must be updated and keep passing.
- Bump `arena0_program::ABI_VERSION` (21 → 22) and the execution profile
  version (2 → 3) exactly once, in step B2. Later steps keep 22/3.
- Delete code; do not deprecate, feature-gate, or leave compatibility shims,
  `#[allow(dead_code)]`, or "removed" comments. Delete tests that only exercise
  deleted behavior; rewrite tests whose behavior survives.
- Match surrounding style: comment density, naming, doc comments on public
  items. Keep doc comments truthful after the change (grep for stale mentions of
  deleted concepts in comments and docs).
- Never log private payloads, callout answers, keys, signatures, or program
  bytes.
- Rebuild guests after SDK/macro/ABI changes (`just build-programs`) before
  diagnosing integration failures. `REQUIRED_FUNC_EXPORTS` owns required export
  names; update its independent Wasm test fixtures when the ABI changes.
- Do not commit, push, create branches, or rewrite git history. The integration
  owner commits.
- Before finishing, run: `cargo fmt --all`, `just check` (fmt + clippy), and
  `just test`. Both must pass. If a failure is pre-existing (also fails on the
  base commit) say so with evidence; otherwise fix it.
- Report in your result: what changed per crate, deletions, test changes,
  commands run with pass/fail, any deviation from this design with the reason,
  and any open question.

## Vocabulary

- **Callout**: the program's single open question to its controlling agent.
- **Divergence**: a writer message that the receiving participant's program
  rejects or whose advertised post-state it does not reproduce.
- Never call either "non-recoverable". "Recovery" means crash restart.

---

## Step A — architecture and proposal docs

Revise `docs/protocol-architecture.md`, `docs/proposals/remove-dual-session-execution-path.md`
and `docs/programming.md` (and `docs/technical-overview.md` if it repeats the
same facts) to the target behavior below. Describe behavior and invariants, not
future code names that do not exist yet. Keep the docs' voice and terminology
rules (participants, Host only for implementation).

Target behavior to document (replace the contradicting text; do not append
"changes" sections):

1. Events: `SessionStarted`, `MessageReceived`, `InputReceived`, `TimerFired`
   (one timer event with a typed payload; an untyped timer carries the unit
   payload), `React`. No `Signed` event, no continuation tags, no async
   `.await` lowering.
2. Effects: `SessionEnd`, `SessionAbort`, `Fail`, `Broadcast`, `SetTimer`.
   No `Callout`, `Sign`, or `RetryInput` effects. Program-authored abort/fail
   stay (they are the terminal effect of an agreed step).
3. Callouts are derived from state. After every accepted dispatch the program's
   read-only `callout` function computes at most one open callout from the
   resulting state image. The open callout is stored with that image: in the
   committed execution record, or in the staged shared proposal until it is
   certified. Same callout index and same context keeps its `PendingId`; a
   different callout replaces it with a new `PendingId`; none withdraws it. A
   terminal status has no open callout. An open callout is never a lock: every
   event, including `React`, keeps dispatching while it is open. `React` runs
   once per agreed step.
4. An answer (`exec.submit`) must name the exact open `PendingId`; otherwise
   `CalloutNotPending`. While a staged proposal carries the answered result the
   answered callout stays open and a resubmission gets `AgreementPending`.
5. Bad agent input never ends a session. If the program rejects an answer
   (decode error, `on_input` error, trap, or resource limit inside the input
   handler), nothing is persisted, both state memories are restored, the
   callout stays open with the same `PendingId`, and `exec.submit` returns an
   `InputRejected` error carrying the program's bounded reason. There is no
   retry effect. The Host validates the answer against the callout's output
   schema before dispatch; the guest decodes fallibly.
6. Guest signing is a synchronous host call available only in local handlers
   (`InputReceived`, `TimerFired`, `React`), never in `SessionStarted` or
   `MessageReceived` dispatches or read-only projections. It signs a versioned,
   execution-bound `GuestSignData` preimage (domain, version, session, program
   hash, execution id, event position, call ordinal within the dispatch,
   scheme, payload) with the participant's Ed25519 identity or execution BLS
   key and returns both the exact signed bytes and the signature to the guest.
   Both schemes are deterministic, so rerunning the handler after a crash
   yields the same signature. Declared `Sign { schemes }` capabilities still
   gate the import.
7. Divergence ends the session: the participant that detects it records a
   Host-signed `Fail` occurrence at its agreed cursor (the existing
   authenticated failure path) and its peers receive it as an `Abort` frame.
   Invalid frames (wrong writer, wrong pre-state, stale position, mismatched
   message id) are dropped, not divergence.
8. A participant accepts a peer's authenticated abort/failure occurrence at its
   agreed cursor even if it has signed a staged proposal, unless that peer's
   own signature is in the staged proposal. It still refuses its own stop after
   it has signed (a certificate for the staged step could exist).
9. Storage target (section 8 and the durable delivery section): the execution
   actor is the only writer of its execution's data. It holds the committed
   execution state, applies protocol transitions itself, and hands complete
   records to the store, which only persists them in one SQLite transaction per
   transition. No store owner thread, command queue, write-through working set
   or cache, compare-and-set retry loops, or outbox leases. The store keeps a
   version check only as a corruption tripwire. Replace the 64 MiB working-set
   paragraph and the owner-thread paragraph accordingly.
10. Delivery target: no inbox or outbox tables. Under N-of-N agreement a
    participant stages step s+1 only after certifying step s, so the most any
    peer can lack from me is: my certificate for my last agreed step, my staged
    message and my signature on it, my terminal signature or the terminal
    certificate, or my abort occurrence. All are already in execution state.
    The actor resends these current frames to each peer independently and
    tracks each peer's acknowledgement in memory; after a restart it simply
    resends. Signature frames carry certificates (commitment, signer set,
    aggregate) once available, so nothing is re-signed; a participant signs a
    commitment only after it is durable. A receiver acknowledges a frame only
    after its committed apply or a duplicate/stale decision; a frame it cannot
    apply yet gets a retryable "not yet" and nothing is stored. A message
    counts as applied once its proposal is durably staged. Each peer has its
    own send lane with a bounded send deadline; one unresponsive peer cannot
    stall others.
11. Terminal notification: the finished observation is emitted when the
    receipt is published (a local fact). The actor stays alive until every
    peer has acknowledged its final frames, then records one durable "final
    frames delivered" fact and retires. Startup resumes a finished execution
    only if that fact is missing. The daemon supervisor no longer stops the
    actor on the finished observation.
12. Direct messages (future, with blobs): per-recipient sequence numbers, a
    small queue of unacknowledged sends whose payload stays in the event
    record, the receiver's last applied sequence per sender committed with the
    handler result, attachments pinned until acknowledged. Mention only as a
    forward-looking note where the doc discusses delivery; do not design more.
13. Remove the `PendingRecord`/`PendingOperation` paragraph, the continuation
    tag text, the Callout outbox context/re-emission text, and the "competing
    answers" paragraph's reliance on the actor queue (keep: an answer that
    loses reports `CalloutNotPending`; a lost response is an unknown outcome
    that must not trigger automatic resubmission).
14. Update `ABI_VERSION = 21` to 22 and "execution profile is version 2" to 3.
15. In the proposal doc, rewrite the retry, input-fault, continuation, timer
    and signing passages to match 1–8; keep its structure.
16. In `docs/programming.md`, document the `callout` function, `on_input`
    returning a plain error (`anyhow::Result`) that rejects the answer, one
    `on_timer`, and synchronous `ctx.sign(...)` in local handlers. Remove
    `.await`, `.pending(...)`, `#[arena0::pending]`, `.retryable()`, and
    `InputFault`.

Checks for step A: `git diff --check`. No Rust build needed.

---

## Step B2 — delete async lowering, the trait form, pending labels, Notification

1. Macros (`crates/arena0-sdk-macros`):
   - Delete `program/continuations.rs`, `program/host_async.rs`,
     `program/trait_form.rs` as program forms. `#[arena0::program]` accepts only
     the inline-module shell; an `impl` block is a compile error with a clear
     message. Move the minimal shared codegen that `module_shell.rs` still
     needs from `trait_form.rs` into `module_shell.rs` (or a small helper
     module); delete everything only the impl form used. Update
     `program/mod.rs` docs.
   - Delete `attr_pending.rs` and the `#[arena0::pending]` attribute.
   - Remove `__arena0_restore_continuation` and continuation-tag handling from
     `guest_abi.rs`.
   - Update macro tests (`program/tests.rs`) and trybuild/UI tests if any.
2. SDK (`crates/arena0-sdk`): delete `ArenaFuture`, `ArenaFutureEffect`,
   `callout_typed`, `into_arena_future`, `CalloutBuilder::pending`,
   `CalloutBuilder::__continuation_tag`, `SignBuilder::pending`,
   `SignBuilder::__continuation_tag`, `Arena0Pending`, `PendingDecl`,
   `PendingKind`/`PendingRecord` re-exports if they become unused by the SDK,
   `Program::__arena0_restore_continuation`, and the `set_continuation_tag`
   import. Keep `CalloutBuilder::dispatch`, `SignBuilder::dispatch` and
   `expected_type` for now (later steps delete them). Update docs that mention
   `.await`.
3. Protocol: remove `continuation_tag` from `Event::InputReceived`,
   `Event::Signed`, `Effect::Callout`, `Effect::Sign`, `PendingRecord`; remove
   `pending_label` from `Effect::Callout`, `Effect::Sign`, `PendingRecord`.
   Remove `SessionMessage::Notification` (node) and `FrameId` if it has no
   other producer, plus their consumers (daemon exec_manager, store, protocol
   `execution/effect.rs`).
4. Sandbox: delete the `set_continuation_tag` import, `next_continuation_tag`,
   `request_input_pending` and `sign_pending` imports (their only extra fields
   were label/expected type); keep `request_input` and `sign` with the
   remaining fields. Update `abi` import-name constants and
   `REQUIRED_FUNC_EXPORTS` fixtures if affected.
5. Node/store/daemon: delete continuation-tag plumbing and pending-label
   columns/fields. Store schema: drop the corresponding columns in place
   (bump the store schema version constant once in this step if the schema
   text changes; later steps reuse it).
6. Programs: remove `#[arena0::pending]` enums and `.pending(...)` calls
   (rock-paper-scissors, chess). Behavior unchanged.
7. Bump ABI_VERSION to 22 and execution profile version to 3. Step A owns
   every file under `docs/` except `docs/api/`; code steps edit `docs/api/`
   only where their API changes.

---

## Step B5 — one timer event

1. Protocol: `Event::TimerFired { timer: TimerPayload }` replaces `TimerFired`
   and `TypedTimerFired`; `Effect::SetTimer { delay_ms, timer: TimerPayload }`
   (not optional). An untyped timer is `TimerPayload` for the unit type
   (define one constructor, e.g. `TimerPayload::unit()`; keep bounded Borsh).
   Renumber event/effect tags densely.
2. Sandbox: one `set_timer(delay_ms, type_ptr, type_len, data_ptr, data_len)`
   import; delete `set_typed_timer`.
3. SDK/macros: one `Program::on_timer(ctx, timer: TimerPayload)` trait
   method. The module shell accepts `on_timer(ctx)` (payload ignored) or
   `on_timer(ctx, timer: T)` (decoded with `decode_timer_payload`); generate
   the one trait method in both cases. `ctx.effects().set_timer(timer, schedule)`
   keeps its authoring surface; untyped `set_timer(ms, ())` produces the unit
   payload.
4. Store/node: `active_timers` timer payload column becomes non-null; one
   firing path.

---

## Step B1 — retry is a plain rejection; bad input never ends a session

1. ABI (`arena0-program/src/abi.rs`): `DispatchOutput { status: CallStatus,
   reason: Option<String> }` with `MAX_REJECTION_REASON_BYTES = 1024`
   (bounded Borsh; reason present only when `status == Rejected`; reject a
   present reason with `Accepted`).
2. SDK:
   - Delete `InputFault`, `Retryable`, `retryable!`, and their re-exports.
   - `Program::on_input(ctx, input) -> anyhow::Result<ProgramTransition<Self>>`.
     Any `Err` rejects the answer.
   - `Arena0Callout::from_raw(callout_index, data) -> anyhow::Result<Input>`;
     `__parse_input_data` becomes fallible; unknown index is an `Err`.
   - `CalloutSpec::decode` returns `anyhow::Result`.
   - Delete `host_retry_input` and the `retry_input` import.
3. Macro glue (`guest_abi.rs`) for `InputReceived`: decode fallibly; on decode
   error or `on_input` error return `DispatchOutput { status: Rejected,
   reason: Some(truncated "{error:#}") }` without storing state. No panics on
   agent-controlled data in this arm. (Traps elsewhere are unchanged.)
   `MessageReceived` rejection keeps `reason: None` unless trivially available.
4. Protocol/sandbox/store/node: delete `Effect::RetryInput` (+tag), the retry
   branches in `validate_pending_dispatch`/`next_dispatch_status`, store
   `cancel_retry_effects`/retry filters, `EffectKind::RetryInput`, the node
   outbox RetryInput arm and the RetryInput check in `dispatch_event`.
   Sandbox: `DispatchResult` (or equivalent) carries the rejection reason.
5. Node:
   - `ExecError::InputRejected(String)` ("callout answer rejected: {0}").
   - `dispatch_event` returns a type that distinguishes committed / frozen /
     rejected-with-reason (replace `Option<bool>` with a small enum, e.g.
     `DispatchOutcome::{Committed, Frozen, Rejected { reason }}`).
   - For an `InputReceived` dispatch only: a sandbox error (guest trap, fuel
     or memory limit) restores the resident from the committed images and is
     reported as `Rejected { reason: "input handler trapped: …" }`, never as a
     fatal actor error.
   - `submit_input` returns `Err(SubmitInputError::Expected(InputRejected))`
     for a rejection. It returns `Ok` only after a committed dispatch.
6. Daemon/API: `ApiErrorCode::InputRejected`; map `ExecError::InputRejected`
   to it with the reason as message; emit `SessionCalloutAnswered` only on
   `Ok`. Update `docs/api` (json-rpc.md and events) for the new error code.
7. Programs: `.retryable()?` → `?`, `InputFault::Retryable(x)` → `x`,
   `Result<_, InputFault>` → `anyhow::Result<_>`; chess, contract-net,
   rock-paper-scissors, prisoner-dilemma, vickrey-auction, minimal-program.
   SDK testing harness: replace `FaultStatus::Retryable` with a rejection
   status carrying the reason; update program tests accordingly.
8. Tests to add (real owning operations, not reimplementations):
   - node: an answer the program rejects returns `InputRejected`, persists
     nothing (version unchanged), keeps the same open `PendingId`, and a
     subsequent valid answer commits.
   - node: an answer that makes the input handler trap is rejected the same
     way and the session stays live.
   - daemon or tests crate: `exec.submit` with a rejected answer returns the
     `InputRejected` code and no `callout_answered` event.

---

## Step B3 — guest signing is a synchronous host call

1. Protocol: delete `Effect::Sign`, `Event::Signed`,
   `PendingOperation::Sign`/`PendingKind::Sign`. Keep `GuestSignData` but
   rename `effect_index` → `call_index` (ordinal of the sign call within one
   dispatch, starting at 0); bump `GuestSignData::VERSION` to 3 and the domain
   tag to `arena0/guest-sign/v3`.
2. Sandbox:
   - New trait in `arena0-sandbox`:
     ```rust
     pub trait GuestSigner: Send + Sync {
         /// Sign one guest payload. Returns (exact signed bytes, signature).
         fn sign(&self, call_index: u32, scheme: SignScheme, payload: &[u8])
             -> Result<(Vec<u8>, Vec<u8>), String>;
     }
     ```
   - `DispatchCall::with_signer(Arc<dyn GuestSigner>)`; stored in `HostState`
     for that dispatch with a per-dispatch call counter.
   - Import `sign(scheme: u32, data_ptr, data_len, out_ptr, out_cap) -> u32`:
     checks capability scheme, lifecycle Active, not read-only, signer
     present (else trap "sign is only available in local handlers"), calls the
     signer, writes Borsh `(Vec<u8> signed_bytes, Vec<u8> signature)` into
     `out_ptr` if it fits in `out_cap` (else trap), returns bytes written.
     Delete the old `sign` effect import. Define
     `SIGN_RESULT_OVERHEAD_BYTES` in `arena0-program` (large enough for the
     `GuestSignData` header, two length prefixes, and a 64-byte signature;
     assert it in a test).
3. SDK: `ctx.sign(scheme, payload) -> Signed { signed_bytes: Vec<u8>,
   signature: Vec<u8> }` (sync; allocates `payload.len() +
   SIGN_RESULT_OVERHEAD_BYTES`). Delete `SignBuilder` and `effects().sign`.
   Native (non-wasm) builds: the SDK testing harness supplies a deterministic
   fake signer; keep it minimal.
4. Node: implement `GuestSigner` for a small struct built per dispatch with
   (session hash, program hash, exec id, event position, `NodeKeys`/execution
   key handles). It builds `GuestSignData::new(..., call_index, scheme,
   payload)` and signs `signing_bytes()` with Ed25519 identity or execution BLS
   key. Pass it only for `InputReceived`, `TimerFired` and `React` dispatches.
   Delete `sign_and_resume`, `resume_signature`, `validate_guest_sign_data`,
   the Sign outbox arm, `OutboxDrainSummary::sign_consumed` and the progress
   trampoline (`MAX_PROGRESS_PASSES` loop collapses to one pass).
5. Store: delete Sign outbox rows/kinds, `PendingRequest::Signature`, and
   related tests.
6. Tests: sandbox test for the import (signer absent → trap; capability
   scheme gating; result round-trip); node test that a React handler signing
   with Ed25519 returns bytes that verify under the participant's key and that
   a MessageReceived handler calling sign traps.

---

## Step B4 — the open callout is derived from state

1. SDK/macros:
   - `Program::callout(ctx: &Context<Self::Shared, Self::Local>) ->
     Option<Self::Callout>` with default `None`; the module shell forwards a
     module `fn callout(ctx: &Context) -> Option<Callout>` if present.
   - Dispatch glue: after an accepted handler (before storing state), compute
     `P::callout(&ctx)` and return it in `DispatchOutput.callout:
     Option<CalloutRequest { callout_index: u32, context: Vec<u8> }>` where
     `context` is `serde_json::to_vec(&callout)` (same JSON as today's
     per-variant struct). Bound `context` by the existing callout payload
     bound.
   - Delete `Effects::callout`, `CalloutBuilder`, `host_callout_raw`,
     `request_input` import, `expected_type` machinery
     (`Arena0CalloutRequest::expected_type_name` if unused).
2. Protocol:
   - Delete `Effect::Callout`, `ExecutionStatus::Waiting`, `PendingRecord`,
     `PendingOperation`, `PendingKind`, `validate_pending_record`, and
     `ExecutionStatus::pending()`.
   - New `OpenCallout { id: PendingId, callout_index: u32, context: Vec<u8> }`
     (bounded Borsh). `ExecutionState` gains `callout: Option<OpenCallout>`;
     `SharedProposal` gains `callout: Option<OpenCallout>` installed at
     certification with its memories (and copied into the deferred broadcast
     successor).
   - `pending_id(exec_id, event_position)` (drop the effect ordinal).
   - `apply_dispatch(event, shared, local, effects, terminal_outcome,
     pending_id, callout: Option<CalloutRequest>)`:
     - `InputReceived` requires `pending_id == Some(open.id)` and
       `callout_index == open.callout_index`; every other event requires
       `pending_id == None`. Mismatch → `PendingContinuationMismatch` (rename
       to `CalloutMismatch`).
     - Next open callout: if the dispatch leads to a terminal status → `None`;
       else if the event is not the InputReceived that answered it and
       `callout` equals the current open callout's (index, context) → keep the
       current `OpenCallout` (same id); an answered callout is consumed, so a
       re-ask after an answer gets a fresh id; else if `Some` → new
       `OpenCallout` with `pending_id(exec_id, event_position)`; else `None`.
       For a staged proposal the result is stored in the proposal and the
       committed `ExecutionState.callout` is unchanged until certification.
     - Terminal statuses and `stop`/`interrupt_terminal` clear the callout.
       `validate_recovered` enforces: terminal ⇒ no callout.
   - `ExecutionState::lifecycle()` returns `ExecLifecycle::Waiting` when the
     status is `Active` and a callout is open (keeps the public projection).
   - `InputReceived { callout_index, data }` unchanged otherwise.
3. Sandbox: validate `DispatchOutput.callout` context against the program's
   callout input schema (reuse the existing `callout_inputs` validation that
   `request_input` used); index out of range → dispatch error.
4. Node:
   - Delete React suppression (`status().pending().is_none()` gate) and the
     pending check for `InputReceived` in `dispatch_event` (replace with the
     open-callout id check).
   - `submit_input(pending_id, data)`: look up the open callout from the
     committed state; `CalloutNotPending` if it does not match; the callout
     index comes from the open callout (drop the caller-supplied index if the
     daemon no longer needs it).
   - Announce: the actor keeps `announced_callout: Option<PendingId>` in
     memory; after each progress pass, if the committed state's open callout
     id differs from `announced_callout` and is `Some`, emit
     `SessionMessage::CalloutRequested { pending_id, callout_index, context }`
     (drop `expected_type`) and record it. Restart starts with `None`, so the
     current callout is re-announced once. Delete
     `emit_recovered_pending_requests` and the recovery capture of
     acknowledged callouts.
   - Delete the Callout outbox arm and `callout_requested` helper if unused.
5. Store: delete Callout effect outbox rows, `PendingRequest`,
   `pending_requests`, `list_pending_requests`, `acknowledge_pending_effect`
   and anything only they used. The open callout persists inside the
   execution aggregate bytes.
6. Daemon/API: read the open callout from the execution state
   (`state.callout()`) for status projection, `exec.next`, `submit` and event
   projection; drop `expected_type` from `PendingCalloutStatus` and
   `SessionMessage`; update `docs/api`.
7. Programs: every callout site moves from handlers into `fn callout(ctx:
   &Context) -> Option<Callout>` computed from state (rock-paper-scissors,
   prisoner-dilemma, vickrey-auction, chess, contract-net, minimal-program,
   and any other). Handlers stop emitting callouts. Keep each program's
   observable behavior: the same question is asked in the same situations.
   Where a primitive helper needs `&mut Context`, add a read-only accessor.
8. Tests:
   - protocol unit tests for the id rule (same → keep, different → replace,
     none → withdraw, terminal → none, staged proposal carries it until
     certification).
   - node: React runs while a callout is open; a second question from a
     non-answer handler replaces the first (no session failure); recovery
     re-announces the committed callout once.
   - existing end-to-end program tests keep passing.

---

## Step B6 — divergence ends the session; peers accept its abort

1. Node `apply_message`: after the frame passes validity checks (terminal,
   proposal-frozen wait, future-position wait, stale/prestate/message-id
   mismatch → drop, wrong writer → drop), a guest rejection, a trap/limit
   error during the `MessageReceived` dispatch, or a post-state mismatch is a
   divergence: return `ExecError::Diverged(reason)` where reason names the
   step and cause ("diverged at step {s}: program rejected the writer message"
   / "…: post-state mismatch"). The actor's existing failure boundary
   (`fail_terminal` → `fail_execution`) records the Host-signed `Fail`
   occurrence and delivers the `Abort` frame. Do not mark the inbox row
   rejected for these cases.
2. Protocol `ExecutionState::stop`: replace the `SharedProposalSigned` rule
   with "refuse if the occurrence sender's signature is in the staged
   proposal"; add unit tests for: peer abort accepted after local signing;
   peer abort refused when that peer signed; local stop refused after local
   signing (unchanged).
3. Delete never-constructed `SessionTermination` variants (`Diverged`,
   `ReplayDesynced`, and any other with no producer outside tests).
4. Tests: an end-to-end or node test where one participant's program rejects
   the writer's message: the receiver ends Failed with a divergence reason, and
   the writer ends Aborted/Failed (not wedged) within the test timeout.

---

## Step T — the certified final step is the terminal evidence (before C1)

Owner decision (2026-09-24): delete the terminal signing round. The step that
emits `SessionEnd` always requires agreement; its `StepCommitment` binds the
entry (including `terminal: Some(SessionEnd { outcome })`) and the final
`post_state`, so its N-of-N certificate already is the terminal evidence —
the same rule `StopCause::Shared` uses for program abort/fail. Docs are
committed (c349595); code follows them.

1. Protocol:
   - Delete `TerminalCommitment`, `TERMINAL_DOMAIN`, `TerminalCertificate`,
     `ParticipantTerminalSignature`, `TerminalProof`, the receipt-header
     `SessionTerminal` (trace/commitment.rs; the unrelated
     `arena0_api::SessionTerminal` event type stays),
     `ExecutionState::{add_terminal_signature, interrupt_terminal,
     terminal_pending, pending_terminal, terminal_certificate}`,
     `ExecFrame::End` and its wire variant/tag, and every validation/error
     variant only they use.
   - `ExecutionStatus`: replace `TerminalProof { proof }` with
     `Ended { outcome: TerminalOutcome }` (final step certified, receipt not
     yet published) and `Completed { proof: PublishedProof }` with
     `Completed { outcome: TerminalOutcome, receipt_id: ReceiptId }`; delete
     `PublishedProof` and `Incomplete`. The certification that commits a
     `SessionEnd` step moves straight to `Ended` (one transition, no
     intermediate state). `ReceiptWork` becomes
     `{ NotTerminal, Assemble, Published }`. `ExecLifecycle::Incomplete` is
     deleted (protocol, API, daemon, CLI, TUI) since nothing can produce it;
     `Ended` projects as `Active` exactly as `TerminalProof` did.
   - Receipt: `ReceiptTermination::Completed` carries no fields. Assembly
     and validation require the last trace entry to have
     `terminal == Some(SessionEnd { outcome })` with `outcome` equal to the
     receipt's outcome bytes; its agreement is checked like every entry.
     `RECEIPT_VERSION` 3 → 4 and the id domain `arena0/receipt/v4`.
2. Verify (`arena0-verify` light): completion is the rule above; there is no
   terminal signature check.
3. Node: delete `ensure_terminal_signature`, End-frame inbox resolution, the
   CollectSignatures branch of `progress`, and the `interrupt_terminal` arm
   of `fail_execution` (an `Ended` execution is preserved; assembly
   retries). The outbox never produces End frames.
4. Store: delete `commit_terminal_signature`, `interrupt_terminal`, their
   commands, any rows/validation for terminal signatures, and use the new
   statuses in assembly and publication. `SCHEMA_VERSION` 3 → 4 (C3 keeps 4).
5. Tests: delete terminal-signature tests; add protocol tests that
   certifying a SessionEnd step yields `Ended` with the outcome, and that a
   receipt whose last entry is not a certified SessionEnd, or whose outcome
   differs, fails validation. Every end-to-end program test stays green.

---

# Stage C — actor-held state, plain store, state-driven delivery

Owner decisions: the actor holds execution state; the store is plain (no
owner thread, command queue, byte budget, write-through cache, CAS retry
loops, leases, inbox, or outbox); delivery is state-driven with certificate
frames; the finished notification is emitted at receipt publication and the
actor stays alive until peers ack its final frames; a durable
`frames_delivered` fact drives resume. `docs/protocol-architecture.md`
§"Durable delivery" and "Terminal proof and publication", and
`docs/technical-overview.md` §"Persistence and recovery" already describe the
target; code follows them. Three commits: C1, C2, C3, in order.

## Step C1 — the store is a mutex around one connection

1. `arena0-store`:
   - `StoreHandle { inner: Arc<Inner> }`, `Inner { db: std::sync::Mutex<Option<Database>>,
     execution_claims: StdMutex<HashSet<ExecId>>, host_id: PeerId }`.
     `Database` keeps the connection, the `OwnerLock`, lease settings (until
     C3) and `transaction_poison`.
   - One helper: `async fn run<T: Send + 'static>(&self, f: impl FnOnce(&mut Database) -> Result<T, StoreError> + Send + 'static) -> Result<T, StoreError>`
     = `spawn_blocking` on a clone of the Arc, lock the mutex, `None` →
     `StoreError::Closed`, a poisoned database → `Closed` (keep today's
     poison semantics), else call `f`. Every async `StoreHandle` /
     `ExecutionStore` method becomes `self.run(move |db| db.method(args)).await`
     with unchanged public signatures (except the deletions below).
   - `Store::open` / `StoreReservation::open` open, validate and recover on
     the calling thread (they are already sync), then build the handle.
     `Store::shutdown` takes the `Database` out of the mutex (dropping the
     connection and releasing the lock) and returns; later calls get
     `Closed`. Keep its async signature if callers await it.
   - Delete: `Command`, `QueuedCommand`, `owner_loop`, `dispatch_command`,
     the `Semaphore` budget, `StoreHandle::{request, send, command_cost,
     try_send}`, `StoreConfig::{queue_capacity, queue_bytes,
     with_queue_capacity, with_queue_bytes}` and their validation,
     `DEFAULT_QUEUE_CAPACITY`, `DEFAULT_QUEUE_BYTES`, `MAX_COMMAND_OVERHEAD`,
     `USER_AGENT_COMMAND_OVERHEAD`, `StoreError::{CommandTooLarge,
     ReplyDropped, OwnerPanicked}` if nothing else constructs them, and the
     B4 queue-accounting test.
   - Delete the write-through working set: `ExecutionWorkingSet`,
     `WorkingExecution`, `PendingExecution`, `cache_pending`,
     `EXECUTION_WORKING_SET_BYTES`, `Database::{executions,
     pending_execution}`. `load_execution` reads SQLite and decodes; it no
     longer calls `validate_projection_rows` (open-time `validate_database`
     keeps validating every aggregate once).
2. Callers (daemon, node, cli, tests) drop the deleted config builders.
3. Tests: delete tests of the queue, byte budget and working set; keep a
   test that a store call after `shutdown` returns `Closed` and that the
   process lock is released after shutdown.

## Step C2 — the actor holds state; the store persists transition records

1. Protocol: unchanged transitions (`activate`, `apply_dispatch`,
   `add_step_signature`, `add_terminal_signature`, `stop`,
   `interrupt_terminal`, `publish_receipt`). The actor calls them; the store
   never does.
2. Store API (replaces `activate`, `commit_dispatch`, `commit_step_signature`,
   `commit_terminal_signature`, `stop_execution`, `interrupt_terminal`,
   `publish_terminal` on `ExecutionStore`):
   ```rust
   pub struct TransitionRecord {
       pub expected: ExecutionVersion,   // version of the state the actor started from
       pub next: ExecutionState,         // the state after the protocol transition
       pub change: Change,
       pub now_ms: u64,
   }
   pub enum Change {
       Activate,
       Dispatch { event: Event<Vec<u8>>, effects: Vec<Effect>, timer_id: Option<TimerId>, inbox_id: Option<InboxId> },
       StepSignature { certified: Option<SharedProposal>, inbox_id: Option<InboxId> },
       Stop { inbox_id: Option<InboxId> },
       Publish { artifact: ReceiptArtifact },
   }
   impl ExecutionStore {
       pub async fn persist(&mut self, record: TransitionRecord) -> Result<(), StoreError>;
       pub async fn assemble_receipt(&self) -> Result<ReceiptArtifact, StoreError>; // read-only, from durable rows
   }
   ```
   (`inbox_id` fields exist only until C3 deletes the inbox.)
   `persist` runs one transaction: read the row's version; `!= expected` →
   `StoreError::Corruption("execution version moved")` (tripwire, not a
   retry signal); write `next` (state bytes, checksum, indexes); then derive
   the side rows from `change` exactly as the old per-transition functions
   do today (event record, agreed step + released effects/timers/outbox
   frames on certification, establishing/successor frames, timer
   consume/cancel, inbox marks, receipt artifact + relation on Publish).
   Reuse the existing row helpers; delete the protocol calls and
   `validate_dispatch_sources`/`inbox_replay_outcome`/`version_mismatch`
   from the store path. `ApplyOutcome` is deleted.
3. Node:
   - `ExecutionActor` gains `state: ExecutionState`, loaded once in
     `recover` after `ensure_execution` (and after `create_execution`).
     `load_state()` is deleted; readers use `&self.state`.
   - One helper: `async fn persist(&mut self, next: ExecutionState, change: Change) -> Result<(), ExecError>`:
     build the record with `expected = self.state.version()`; on `Ok`
     install `next`; on `Err` reload `self.state` from the store, rebuild the
     resident (`restore_resident`), and return the error.
   - Every transition site computes `let mut next = self.state.clone();
     next.<transition>(...)?;` then `self.persist(next, change)`. Delete the
     CAS retry loops and `MAX_CAS_RETRIES` (guest.rs `dispatch_event`,
     `ensure_step_signature`, `persist_abort`,
     `finalize_receipt`, `fail_execution`, inbox resolution).
   - `fail_execution` and startup recovery keep one node-owned failure
     transition; recovery paths that run without an actor load the state
     from the store once and call the same helper shape.
4. Daemon/API read paths keep reading committed state from the store.
5. Tests: store tests call `persist` with records built from real protocol
   transitions; add a test that a stale `expected` returns `Corruption` and
   writes nothing. Node tests keep passing.

## Step C3 — state-driven delivery, no inbox or outbox, explicit end phase

Owner decisions (2026-09-24), docs committed in 8c035e0
(docs/protocol-architecture.md, "Terminal evidence and publication"):
every participant must reach the same conclusion about the session; ending
the session with peers is an explicit phase of execution state (like a TCP
close handshake), not a durable side fact. There is no `frames_delivered`.

1. Protocol (`arena0-protocol`):
   - Rename `ExecutionStatus::Ended { outcome }` to
     `Certified { outcome }` (final step certified, receipt not yet
     published). Status is now `Active | Certified | Completed | Stopped`
     (plus whatever pre-activation variants exist). Rename everywhere,
     including tests and the daemon mapping.
   - New local end phase, a field of `ExecutionState` (Borsh-encoded with
     the state; never part of a commitment, `StateHash`, or receipt):
     ```rust
     pub enum EndPhase {
         Open,
         Ending { unconfirmed: BTreeSet<PeerId> },
         Ended { unconfirmed: BTreeSet<PeerId> }, // empty = every peer confirmed
     }
     impl ExecutionState {
         pub fn end_phase(&self) -> &EndPhase;
         /// Remove `peer` from the unconfirmed set (Ending or Ended);
         /// Ending with an empty set becomes Ended. Error if the phase is
         /// Open or the peer is not a remote participant; Ok(false) if the
         /// peer was already confirmed (no state change, do not persist).
         pub fn confirm_end(&mut self, peer: PeerId) -> Result<bool, ExecError>;
         /// Ending { u } -> Ended { u }. Error unless Ending.
         pub fn expire_end(&mut self) -> Result<(), ExecError>;
         /// Does this terminal evidence from a peer reach my conclusion?
         pub fn end_conclusion_matches(&self, frame: &ExecFrame) -> EndMatch; // Same | Different | NotTerminalEvidence
     }
     ```
     Every transition that makes the execution terminal (certifying a
     SessionEnd step → `Certified`; any stop → `Stopped`) sets
     `Ending { unconfirmed: all participants except me }` in the same
     transition. Sessions have 2–64 participants, so the set is never empty
     at entry; do not add a single-participant branch. Publication does not touch
     the phase. Bump `version` on each confirm/expire like any transition.
   - "Same conclusion" compares terminal evidence by kind. If my session
     ended with a certified final step (completion, or a certified shared
     `SessionAbort`/`Fail` stop), `Same` iff the peer's frame is a
     `StepCertificate` for that same final step (same commitment). If my
     session ended with a unilateral stop (`StopCause::Authenticated`),
     `Same` iff the peer's frame is an `Abort` whose occurrence stops at my
     agreed cursor (bytes may differ: unilateral reports). Anything else
     authentic but terminal is `Different`. (A certified stop and a
     unilateral stop cannot coexist at one cursor: the certificate needs the
     aborter's signature, and a participant refuses its own stop after
     signing.)
   - `ExecFrame` gains `StepCertificate { certificate: StepCertificate }`
     (wire variant, bounded).
   - `ExecutionState` gains `last_certificate: Option<StepCertificate>` set in
     `commit_shared_inner`, and `certify_step(certificate)`: verify the
     aggregate against the binding and the staged proposal, then run the same
     commit path as the last signature does (including the move to
     `Certified` for a SessionEnd step).
   - `pub fn current_frames(&self, me: PeerId) -> Vec<ExecFrame>`:
     `last_certificate` as `StepCertificate`; for a staged proposal, its
     `Message` if `me` is its writer plus my `StepSignature` if present;
     when stopped by an authenticated occurrence → that `Abort`, whoever
     originated it (it is authenticated by the occurrence's own signature;
     receivers must not require the transport source to be its sender).
   - `pub fn terminal_evidence(&self) -> Option<ExecFrame>`: the FIN — the
     final `StepCertificate` (completion or certified shared stop), or the
     adopted `Abort` for a unilateral stop.
2. Transport: `ExecDeliveryRejection::NotYet` (and
   `TransportError::ExecNotYet`), retryable.
3. Node, new `execution/delivery.rs` replacing `inbox.rs` and `outbox.rs`:
   - Sender: one lane per peer (at most one in-flight send, 5 s deadline,
     then drop the stream and retry on the next tick); an in-memory acked
     set per peer keyed by frame digest, pruned to `current_frames`.
     Non-terminal frames: `Rejected` → log without payload and mark done for
     that peer; `Conflict` → terminal error as today; `NotYet`/timeout/
     stream error → retry.
   - Ending (phase `Ending`, or `Ended` with unconfirmed peers after a wake):
     each lane to an unconfirmed peer sends `terminal_evidence()` until:
     `Ok` ack → `confirm_end(peer)` and persist (`Change::End`);
     `Rejected`/`Conflict` → invariant violation: log an error without
     payload, stop sending to that peer for this actor run, keep the other
     lanes running; the peer stays unconfirmed. Never an actor error: one
     faulty peer must not cut delivery to honest ones.
   - Receiver: classify against `self.state`; ack only after the resulting
     transition is persisted. `Rejected` is reserved for frames that fail
     authentication or binding checks; a receiver never rejects authentic
     terminal evidence.
     | frame | ack | NotYet | Rejected / Conflict |
     |---|---|---|---|
     | Message | seq < agreed, or the same commitment is staged; seq == agreed with no proposal and valid → dispatch, then ack; divergence → fail, then ack | seq > agreed | wrong writer/prestate/id → Rejected; a different proposal staged at seq → Conflict |
     | StepSignature | step < agreed; matches proposal → add; already present | no proposal, or step ahead | commitment mismatch → Rejected |
     | StepCertificate | step < agreed; verifies → `certify_step` | no proposal yet | fails verification → Rejected; verified but contradicts local state → actor error (log without payload), no answer |
     | Abort | valid at cursor → stop; behind cursor (stale) | ahead of cursor | invalid → Rejected |
     When I am already terminal: a peer's terminal evidence with
     `Same` conclusion → `confirm_end(peer)` (persist if changed), then
     ack; `Different` → log an invariant error without payload, answer
     `Conflict`, peer stays unconfirmed; any other authentic frame → ack
     as stale.
   - Terminal: emit the finished message at publication (drop the
     `has_unsettled_frames` gate). End-confirmation window: node config
     `end_confirmation_window: Duration` (default 10 minutes; tests set it
     small), an in-memory deadline from actor start or wake (no wall clock
     in protocol state). While `Ending` and the deadline passes →
     `expire_end()` and persist. The actor returns from `run` when the
     receipt is published and the phase is `Ended` with no unconfirmed
     peers, or `Ended` and this run's deadline passed.
   - Exec router, frame for a session with no live actor:
     local execution `Ended` with the sender unconfirmed → answer `NotYet`
     and resume the actor (it sends the terminal evidence again; the
     sender's retry reaches the live actor); local execution terminal with
     the sender confirmed (or `Ended { {} }`) → ack as stale; otherwise
     drop the stream as today. The router must answer from narrow columns,
     never by decoding the state image.
   - Daemon supervisor (`exec_manager.rs`, finished-message handling) must
     not stop the actor on the finished message.
   - `exec.status` exposes the end phase: `end: { "phase": "open" |
     "ending" | "ended", "unconfirmed": [<peer id>...] }`; update
     docs/api/json-rpc.md (replace the `frames_delivered` text with the end
     phase) and the API types/tests.
4. Store:
   - Delete `database/inbox.rs`, `database/outbox.rs`, the `inbox`,
     `inbox_conflicts` and `outbox` tables, `OutboxId`, `OutboxItem`,
     `LeasedOutbox`, `OutboxStatus`, `OutboxPayloadKind`,
     `OutboxDeliveryOutcome`, `LeaseId`, `InboxId`, `PendingInboxItem`,
     `InboxAcceptOutcome`, `InboxRejectOutcome`, `AuthenticatedFrame`,
     `RecoveryReport` (if only leases), lease config, the related
     `StoreError` variants, `inbox_id` fields of `Change`, and outbox frame
     rows written by `persist`. No `frames_delivered` column or
     `mark_frames_delivered`.
   - `Change::End` (state only, no side rows) for confirm/expire.
   - Derived columns written by `persist_state` in the same UPDATE as the
     state (like `lifecycle`, `agreed_step`): `end_phase INTEGER NOT NULL`
     (0 open, 1 ending, 2 ended) and `end_unconfirmed BLOB NOT NULL`
     (Borsh `BTreeSet<PeerId>`). They are projections of the state, not a
     second authority; the router query reads them. Validate on load that
     they match the decoded state (corruption otherwise).
   - Recovery candidates: nonterminal executions (as today) plus terminal
     executions whose receipt is unpublished or whose `end_phase` is
     `ending`. `Ended` executions are not resumed at startup.
   - `SCHEMA_VERSION` stays 4 (step T bumped it; no migrations).
5. Delete `InflightSend`, `MAX_INBOX_BATCH`, `resolve_pending_inbox`, and
   `OutboxDrainSummary` timing unless reused by the lanes.
6. Tests: receiver classification (each row); sender lane isolation (one
   silent peer does not stall another); restart resends current frames;
   `confirm_end`/`expire_end` unit tests (including idempotent confirm, and
   confirming the last remote peer moves `Ending` to `Ended { {} }`); ack of terminal evidence confirms the
   peer; a peer's matching terminal evidence confirms it (simultaneous end);
   a rejected terminal frame leaves the peer unconfirmed, logs, and does
   not stop delivery to another peer; the window moves `Ending` to `Ended`
   with the silent peer unconfirmed; a frame from that peer wakes the
   retired execution and it then confirms; a confirmed peer's frame to a
   retired execution is acked as stale; startup resumes `Ending` and skips
   `Ended`; the finished message precedes retirement. Existing end-to-end
   tests keep passing.

# Stage E — effects are validated when emitted and applied when the handler returns

Owner decision 2026-09-24 (React is the wrong abstraction). Behavior owner: `docs/protocol-architecture.md` §10 as committed in 86b258a — read it first; it is the spec. Stage E runs before Stage D. One commit. No backward compatibility or migrations (no deployed data), but every in-repo program and fixture must keep working.

## Model (normative summary)

- Agreed events: `SessionStarted`, `MessageReceived`. Only they change shared state or emit lifecycle effects (`SessionEnd`/`SessionAbort`/`Fail`).
- Local events: `InputReceived`, `TimerFired`. They change local state only; a local dispatch whose shared image differs from the agreed shared image is rejected (plain rejection; for input it is `InputRejected` with a bounded reason; never ends the session).
- `React` is deleted everywhere (event, SDK hook, node scheduling, `last_reacted_step`, tests).
- Every effect is validated in its host import at emission, then pushed to `HostState::effect_queue`. The queue is applied only if the handler's result is accepted; reject/trap discards it (already the case).
- `Broadcast` from any event appends to a durable, bounded per-execution outgoing queue. Deferred broadcast (successor proposal with pre==post) is deleted. A local event never stages a proposal.
- Authoring: when status is Active, no proposal is staged, the outgoing queue is non-empty and `writer(agreed shared) == me`, the actor dispatches `Event::MessageReceived { from: me, msg: outgoing[0] }` (the new two-field dispatch event, see "Event split") on its own resident instance — the same dispatch receivers run. Accepted → stage the proposal (pops outgoing[0]; appends any new broadcasts) and deliver the message frame. Rejected or trapped → pop outgoing[0] durably, `tracing::error!` with exec_id only (no payload), continue with the next queued message.
- Local events do not dispatch while a proposal is staged (unchanged). `submit_input` while staged returns `AgreementPending` (unchanged error, new meaning: busy).

## Emission rules (sandbox, `HostState::record_effect` + imports)

Replace `HostState.lifecycle: arena0_protocol::Lifecycle` with `dispatch: DispatchKind { Agreed, Local }` (sandbox-internal), set in `DispatchCall::into_input` from the event (`SessionStarted`/`MessageReceived` → Agreed, else Local). Delete `arena0_protocol::Lifecycle` and `reject_if_lifecycle_disallowed` if nothing else uses them.

| effect | rule at emission | violation |
|---|---|---|
| `SessionEnd`/`SessionAbort`/`Fail` | Agreed only; at most one lifecycle effect per dispatch; not if a `SetTimer` is already queued | trap |
| `SetTimer` | any event; not if a lifecycle effect is already queued | trap |
| `Broadcast` | any event; `queued_before + broadcasts_in_this_dispatch < MAX_OUTGOING_MESSAGES` | return status `1` (queue full) to the guest, queue nothing |

Existing byte/count limits in the ledger stay as they are (they trap). `DispatchCall::with_outgoing_len(n: usize)` supplies `queued_before`; the node passes `state.outgoing().len()` (for an own-message dispatch pass the length *after* removing the message being authored).

ABI: the `broadcast` import becomes `(ptr: u32, len: u32) -> u32` (0 = queued, 1 = queue full). Update the import-signature table in `crates/arena0-sandbox/src/validation.rs`, the independent Wasm test fixtures (WAT in sandbox tests/program.rs), bump `ABI_VERSION` and `EXECUTION_PROFILE_VERSION` in `arena0-program`, rebuild guests.

## Protocol (`arena0-protocol`)

- `Event::React` and `EVENT_REACT` deleted (keep other tags' values explicit; renumbering is allowed, no compat). See "Event split" below for the new `Event`/`StepEvent`/`StepTerminal` types.
- `pub const MAX_OUTGOING_MESSAGES: usize = 16;` next to the other execution limits.
- `ExecutionState`: delete `last_reacted_step` (+ accessor, encode/decode, validation). Add `outgoing: Vec<Vec<u8>>` (FIFO, local, never in `StateHash`/commitments/receipts) with `pub fn outgoing(&self) -> &[Vec<u8>]`. Encode it in the state body; on decode validate `len <= MAX_OUTGOING_MESSAGES` and each message within the existing max broadcast/message bytes bound. `SharedProposal` carries the post-dispatch `outgoing` value and it is installed at certification, like its local state.
- `apply_dispatch` becomes:
  - Local event: `post_state != agreed_state` → new `ProtocolError::LocalSharedChange`. Lifecycle effects cannot be present (sandbox traps); keep a cheap `debug_assert!`-free defensive `Err` only if it costs one line. Append broadcasts to `outgoing` (error `OutgoingQueueFull` if over the bound — defensive, the sandbox already enforced it). Install immediately (`install_dispatch`), never stages.
  - Agreed event: always stages a proposal (as today for portable events), with terminal from the lifecycle effect, effects (timers) staged, and `outgoing` = current outgoing (minus the authored head for an own message) + this dispatch's broadcasts.
  - Own message: `Event::MessageReceived { from, msg }` with `from == self.producer`. Require `msg == outgoing[0]`, else `ProtocolError::NotQueuedMessage`. Build the entry from `(agreed_step, agreed_state, post_state, StepEvent::Message { from, data: msg })` and produce the establishing `ExecFrame::Message { seq, prestate, poststate, data }` from it.
  - Peer message: the node builds `Event::MessageReceived { from, msg: data }` from the frame; the entry is built the same way from the frame's coordinates. Author and receivers therefore build byte-identical entries by construction.
  - Delete: `deferred_broadcast_successor` and its installation in certification, `reacted_step`, the local-event→proposal branch of `normalize_proposal_event` (keep only what builds the own-message trace event/frame), the "shared change needs broadcast"/"terminal agreed event cannot defer a broadcast"/"broadcast count"/"lifecycle count"/"terminal + timer" checks (`validate_effects`, `dispatch_lifecycle_effect` keep only what is still needed to extract the terminal outcome).
- New transition `pub fn drop_outgoing_head(&mut self) -> Result<(), ProtocolError>` (error if empty or a proposal is staged).

## Event split (owner decision 2026-09-24, option A; also closes round-1 §3.1)

One type currently serves as guest input and as trace/commitment record, which forces the author to invent a `message_id` before it knows its post-state. Split it:

```rust
// arena0-protocol/src/event.rs — what a guest dispatch receives; also the
// Host-local record of local events. No trace coordinates.
pub enum Event<M = Vec<u8>> {
    SessionStarted { ensemble: Ensemble },
    MessageReceived { from: PeerId, msg: M },
    InputReceived { callout_index: u32, data: Vec<u8> },
    TimerFired { timer: TimerPayload },
}

// arena0-protocol/src/trace/entry.rs — the portable, agreed step event.
pub enum StepEvent {
    SessionStarted { ensemble: Ensemble },
    Message { from: PeerId, data: Vec<u8> },
}
pub enum StepTerminal {
    End { outcome: Vec<u8> },
    Abort { reason: String },
    Fail { reason: String },
}
pub struct TraceEntry {
    pub trace_version: u32,          // TRACE_FORMAT_VERSION 2 -> 3
    pub step: u64,
    pub event: StepEvent,
    pub pre_state: StateHash,
    pub post_state: StateHash,
    pub terminal: Option<StepTerminal>,
    pub agreement: AggregateAttestation,
}
impl TraceEntry {
    /// Derived on demand; never stored or sent.
    pub fn message_id(&self, session: SessionHash) -> Option<MessageId>;
}
impl StepEvent {
    /// The dispatch event every participant runs for this step.
    pub fn dispatch_event(&self) -> Event<Vec<u8>>;
}
impl StepTerminal {
    pub fn from_effect(effect: &Effect) -> Option<Self>; // lifecycle effects only
}
```

- `MessageId::derive` keeps its inputs (session, from, step, pre, post, data); callers derive it from an entry or a frame when they need it (logs, API, TUI, dedupe).
- `ExecFrame::Message` drops `message_id`: `{ seq, prestate, data, poststate }`. Same for the raw frame in `arena0-wire` (codec + exec.rs) and the transport fixtures.
- Borsh: derive where possible; keep bounded decoding of `data`, `outcome` and `reason` (existing bounds). `TraceEntry` decode no longer needs `validate_shape` (the types make illegal shapes unrepresentable) — delete it, and delete the coordinate/identity re-checks in `execution/validation.rs` (~427-531: position == step, pre_state == entry.pre_state, message-id re-derivation) and the `completed_outcome`/`abort_reason` adapters (use `StepTerminal` methods).
- Bump `TRACE_FORMAT_VERSION` 2→3, `STEP_COMMIT_DOMAIN` v3→v4, the receipt version, and the store schema version if the stored state/proposal encoding has one. No migration.
- Update every consumer: protocol (state.rs, validation.rs, divergence.rs, commitment.rs), node (delivery.rs, guest.rs), store, wire, transport, verify (light.rs), CLI TUI trace decoding (`cli/src/tui/trace.rs`), SDK native harness (fixtures.rs synthesizes `MessageReceived`), arena0-tests (synthetic.rs), docs (`docs/protocol-architecture.md` TraceEntry paragraph, `docs/api/*` if they show the trace/receipt shape).
- Macros: the `MessageReceived` arm reads `{ from, msg }`.

## Store (`arena0-store`)

- `Change::DropOutgoing` (state-only update, like `Change::End`).
- Delete the deferred-successor commit branch in `database/execution.rs` (~745-790) and anything reading `last_reacted_step`.
- The state blob carries `outgoing`; no new columns.

## Node (`arena0-node`)

- `actor.rs::progress`: replace the React branch with `else if self.may_author()? { self.author_next_message().await?; }`.
- `guest.rs`:
  - `author_next_message`: loop while Active, no proposal, outgoing non-empty and `writer_for_shared(agreed shared) == me`: build `Event::MessageReceived { from: me, msg: outgoing[0].clone() }`, `dispatch_event(event, DispatchSource::OwnMessage)`; `Committed` → break (proposal staged; delivery sends it); `Rejected` → `next.drop_outgoing_head()`, `persist(next, Change::DropOutgoing)`, `tracing::error!(exec_id, "own queued message rejected by the program")`, continue.
  - `dispatch_event`: delete the broadcast/lifecycle counting, the shared-change-needs-broadcast check, the writer_state selection and the author check (`guest.rs:447-514`) — emission rules live in the sandbox, the writer check lives in `author_next_message` (own) and in the receive path (peer). Keep the sandbox-hash check and the advertised post-state check for peer messages. Signer only for `InputReceived`/`TimerFired`. Pass `with_outgoing_len`.
  - `DispatchSource` → enum (R1 §3.3): `Local`, `Answer(PendingId)`, `Timer(TimerId)`, `PeerMessage { advertised_post_state }`, `OwnMessage`. Use it wherever the old Option bag was read.
  - A rejected local dispatch due to `LocalSharedChange` is a plain rejection (`DispatchOutcome::Rejected { reason }`), not an actor error.

## SDK, macros, primitives

- `Program`:
  - delete `on_react`.
  - `on_input(ctx: &mut LocalContext<Shared, Local>, input) -> anyhow::Result<()>`.
  - `on_timer(ctx: &mut LocalContext<Shared, Local>, timer) -> Result<(), ProgramFault>`.
  - `on_session_started` / `on_message` keep `Context` (shared mutable) and `Transition`.
- New `LocalContext<Shared, Local>`: same helpers as `Context` except shared is read-only (`shared()`, no `shared_mut`/`state_mut`/`mutate_shared`/`__apply_transition`). It has `local()`, `local_mut()`, `mutate_local`, `effects()`, `sign`, `me`, `ensemble`, `random*`, `log*`, `crypto`, primitive author access.
  - `Context` loses `sign` (agreed handlers cannot sign).
  - Share implementation rather than duplicating: e.g. a private inner struct or a generic `Ctx<Shared, Local, Mode>`. Pick the smallest diff that keeps the public names `Context` and `LocalContext`.
- `Effects::broadcast` and `PrimitiveOutput(s)::broadcast[_via]` return `Result<(), BroadcastError>` (`#[must_use]`); `BroadcastError::QueueFull`. Add `From<BroadcastError>` for `ProgramFault`, `ProtocolFault` and anyhow compatibility. The host shim returns the import status.
- Primitives (`arena0-primitives`, SDK wrappers such as commit_reveal.rs:196-226): author operations (commit, reveal/take_reveal, etc.) read shared, mutate local and return the output. They never apply the author's own message to shared (delete the `self.mutate(|cr| cr.handle(participant, ...))` self-application). `handle` (shared mutation) is used only from `on_message`. Wrappers must be available on `LocalContext` (author side) and `Context` (handle side) as needed.
- Macros (`guest_abi.rs`): remove the React arm; `InputReceived`/`TimerFired` arms build a `LocalContext` from the committed shared (read-only) and never apply a transition; the dispatch returns Rejected for an input handler error as today.
- Rustdoc for `broadcast` (context.rs:675-700) and `sign` rewritten to the new model; `docs/programming.md` handler table and signing paragraph updated (delete `on_react`).

## Programs and fixtures

Rewrite all shipped programs (`programs/*`: chess, contract-net, cumulative-sum, prisoner-dilemma, rock-paper-scissors, sequential-count, vickrey-auction) and the timer fixtures (timer-dispatch-unit, timer-dispatch-typed), plus any other in-repo guest (search for `impl Program for` / `#[arena0::program]`):
- All shared-state logic moves into `on_message` (and `on_session_started`); the author receives its own message there too, so delete author-side duplicates (e.g. RPS `apply_completed_round` in `on_react`).
- What `on_react` did becomes a broadcast queued from `on_message`/`on_session_started` when `ctx.me()` is the participant who owes the next action (e.g. RPS: when the last commit is applied, the expected writer queues its reveal).
- A callout derived only from shared state now stays open until the author's own message is applied. Where that would re-ask an answered question, record the answer in local state and derive the callout from it (RPS already does via `needs_commit(&local)`).
- Ending from a local handler (timer deadline etc.) becomes a queued message whose `on_message` ends the session.
- Program tests keep asserting the same observable game results.

## SDK native harness (`crates/arena0-sdk/src/testing/*`)

Stage D replaces it with a Wasm driver; for now adapt minimally: no React; queued broadcasts are delivered through `on_message` to every participant including the author, in writer order, one at a time; local handlers get `LocalContext`. Keep the public scenario API unless a signature must change.

## Tests (real owning operations, observable results)

- sandbox: lifecycle import from a Local dispatch traps; second lifecycle traps; SetTimer+lifecycle traps (both orders); broadcast returns 1 when `with_outgoing_len(MAX-1)` already has one queued in the dispatch; broadcast works from SessionStarted and MessageReceived.
- protocol: author and receiver build byte-identical `TraceEntry`/`StepCommitment` for the same message (one test through `apply_dispatch` on two states); `TraceEntry` round-trips; `message_id` derivation matches a frame-side derivation; local dispatch with changed shared → `LocalSharedChange`; local broadcast appends to outgoing and commits without a proposal; own message not equal to outgoing[0] rejected; own message stages proposal with outgoing popped and new broadcasts appended; outgoing installed at certification; `drop_outgoing_head`; outgoing decode bound. Delete deferred-successor and react tests.
- node: author waits until it is the writer; authored message staged and delivered after certification of the previous step; own rejected message dropped durably and next one authored; restart with a non-empty outgoing queue authors it; local input that mutates shared state returns InputRejected and changes nothing; SessionStarted may end the session (end/abort now allowed from on_session_started).
- integration (`arena0-tests`): all program scenarios green; two_mcp/open_join/contract_net timings reported vs C3 (open_join 9.77 s, two_mcp 50.30 s, contract_net 5.74 s).

## Verify

`just build-programs`, `cargo fmt --all --check`, `just check`, `just test`. Report changed files, deleted symbols, tests added/removed, commands and results, and timings. Do not commit.

---

# Stage D — last consolidation (after Stage E, base bf49bcf)

Survey: `impl/survey-d.md` (written at ee5bef2). Stage E already closed round-1
§3.1 (StepEvent/StepTerminal), §3.3 (DispatchSource enum), §3.4/§4.1
(PreSession is gone), §3.5 (deferred broadcast deleted), §4.2 (frame carries the
commitment) and §4.3 (terminal + timer traps in the sandbox; architecture
§10 documents it). Stage D is split into commits D1, D2, D3, each implemented,
reviewed and committed separately. The global rules at the top of this file
apply. Do not bump `ABI_VERSION`/`EXECUTION_PROFILE_VERSION` unless the guest ABI
actually changes.

## Step D1 — one identity per Host and dead-code deletions

Owner decision 2026-09-24: one identity per Host. A new identity means a new
Host via `hosts.open`. Delete `Keystore::set_active`, `id.new`, `id.remove` and
the multi-key keystore.

### Keystore (`crates/arena0-daemon/src/store/keystore.rs`)

- The keystore holds exactly one identity: one 32-byte seed in
  `keys/identity.seed` (mode 0600, published with the existing non-overwriting
  `publish_new_private`). No `index.json`, no labels, no active pointer, no
  reconciliation of an identity set, no orphan-seed handling beyond "the only
  allowed entries in `keys/` are `identity.seed` and the MCP signing key file(s)
  that `mcp_auth.rs` already publishes there (check which names it uses) plus
  recognisable interrupted temporaries, which are rejected as today".
- API (replace the current one; migrate callers):
  ```rust
  impl Keystore {
      /// Open the Host identity; `Ok(None)` if `keys/identity.seed` does not exist.
      pub fn open(keys_dir: PathBuf) -> anyhow::Result<Option<Self>>;
      /// Mint the Host identity; fails if one already exists.
      pub fn create(keys_dir: PathBuf) -> anyhow::Result<Self>;
      pub fn peer_id(&self) -> PeerId;
      /// Custody-side signing keys (was `active_crypto`).
      pub fn node_keys(&self) -> anyhow::Result<NodeKeys>;
      pub fn info(&self) -> IdInfo;
  }
  ```
  Keep the private-file helpers (`ensure_private_regular`, `publish_new_private`)
  that `mcp_auth.rs` uses. Delete `KeystoreError` variants that no longer have a
  caller (InvalidLabel, ActiveIdentityRemoval, unknown/ambiguous reference …);
  if only `Storage` would remain, delete the type and use `anyhow`.
- `ensemble.rs` open paths: `open` → `Keystore::open(..)?` then, if `None`,
  `create` when bootstrapping, else bail "no identity in {keys}; start without
  --no-bootstrap". `open_existing` → `open(..)?.ok_or(..)` and the same
  peer-mismatch check.
- `mcp.rs:2102` references `keys/index.json`; point it at `keys/identity.seed`
  (read the surrounding code to keep its meaning).

### API, daemon, CLI, client, docs

- `arena0-api`: delete `HostRequest::{IdNew, IdList, IdRemove}`, `IdRef`,
  `ResponseOk::IdList`. `HostRequest::IdShow` takes no parameters
  (`#[serde(rename = "id.show")] IdShow`) and returns `ResponseOk::Id(IdInfo)`.
  `IdInfo` loses `label` and `active` (keeps `peer_id`, `transport_key`). Update
  the method-name table test in `arena0-api/src/lib.rs`.
- Daemon `server.rs` dispatch: only `IdShow` → `ks.info()`.
- CLI: `arena0 identity` becomes a single command that shows the Host identity
  (delete `IdentityCommand` subcommands New/List/Remove and `identity_reference`;
  keep the JSON/human rendering of one `IdInfo`). Keep the `identity` command
  name.
- `docs/api/json-rpc.md` identity section: one row `id.show | — | Id`, and the
  sentence "Each Host has exactly one identity, minted when the Host is created.
  A new identity means a new Host (`hosts.open`)." Remove the rotation sentence.
  Grep docs/, README.md and crate docs for `identity new`, `id.new`, `id.list`,
  `id.remove`, `label`, `active identity`, `index.json` and fix them.
- Tests: rewrite `crates/arena0-daemon/tests/ensemble.rs` identity cases to
  `id.show`; keystore unit tests keep create/open round trip, permissions,
  refusing a second `create`, refusing unexpected files and interrupted
  temporaries; delete label/active/remove/list tests.

### Dead code

- Sandbox: delete `RandomReplay`, `RandomReplayError`,
  `DispatchCall::with_random_replay`, the lib.rs re-exports, and the replay
  plumbing in `HostState`/`engine/runtime.rs`/`engine/entropy.rs` (the entropy
  source keeps only its live path). Delete their tests.
- Protocol `trace/divergence.rs`: delete `TraceEntry::{validate_format,
  verify_chain, compare_traces, compare_step}` and their unit tests.
  `DivergenceDiagnostic` stays while the SDK harness uses it (D3 decides).
- Verify: delete the `VerifyError` variants with no constructor (ParamsMismatch,
  EmptyTrace, ChainBroken, PublicEntryInvalid, Agreement,
  MissingParticipantAgreement, TerminalNotLast, OutcomeMissing — confirm with rg
  over `crates programs`) and fix the "version-5" doc wording if stale.
- CLI `verify.rs:231-238` re-encodes a parsed artifact to call `verify_light`.
  If `ReceiptArtifact`'s JSON `Deserialize` (`ReceiptArtifact::new`) already
  enforces everything `ReceiptArtifact::decode` plus the `MAX_RECEIPT_BYTES`
  bound enforce, add `pub fn verify_light_artifact(&ReceiptArtifact)` to
  `arena0-verify` (make `verify_light` decode and call it) and use it in the CLI.
  If it does not enforce the size bound, leave the round trip and say so.
- Delete the stale tracked files `ACTOR_LIFECYCLE.md` and
  `.bb/workflows/simplify-execution-model.js` (grep for inbound links first and
  fix any).

### Tests / verify

`cargo fmt --all`, `just check`, `just test` (guests are unaffected; no guest
rebuild needed unless the build says otherwise). Report per the global rules.

## Step D3a — compile each module once, reuse a stable Wasmtime cache, no gossip wait

Owner request 2026-09-25: compile modules once and make sure the Wasmtime cache
is stable and reused across runs. Measured at bf49bcf (RPS, 2 participants, run
alone, 4.4 s total): 1.9 s to negotiation commit, 1.4 s more to
`SessionStarted`, 1.1 s for the 9 agreed steps, 0.2 s for end and receipts. A
cold Cranelift compile costs 0.52 s (RPS) / 0.59 s (chess). A second `load` on
the same engine costs 13 µs. Arena creates a fresh `WasmtimeEngine` for every
load, so it compiles 2 + n times per run, and the per-participant loop at
`arena0-tests/src/arena.rs:488` does this serially. The daemon already does the
right thing (`run.rs`: one `new_persistent` engine; moka cache by
`ProgramHash`).

### One test engine, persistent cache

- `arena0-sandbox`: add a cargo feature `testing` (off by default) exposing
  `pub fn shared_test_engine() -> Arc<WasmtimeEngine>`. It returns one engine
  per process (`OnceLock`), built with `WasmtimeEngine::new_persistent(dir)`
  where `dir` is `$ARENA0_WASMTIME_TEST_CACHE` if set, else
  `<target dir>/wasmtime-cache`. The target dir is `$CARGO_TARGET_DIR` if set,
  else `<workspace root>/target` (derive the workspace root from
  `env!("CARGO_MANIFEST_DIR")` at compile time). Create the directory if
  missing.
- Every test and test-support site that builds an engine only to load a
  program uses it: `arena0-tests` (arena.rs ×3, fixtures.rs ×3), daemon test
  modules (mcp.rs, ensemble.rs, server.rs, catalog.rs, run.rs,
  open_host_tests.rs), node tests, sandbox unit tests. The rule: tests share
  it unless they specifically test engine configuration or cache behaviour
  (for example `program.rs:400-411`, which keeps its own temporary cache
  directory). Add `arena0-sandbox = { …, features = ["testing"] }` to the
  dev-dependencies that need it. `rg 'WasmtimeEngine::new\(\)'` should then
  find only production sites (`cargo-arena0`, which is a one-shot CLI and may
  stay uncached) and the cache/config tests.
- Arena: load the program once per run through the shared engine and pass the
  same `Arc<LoadedProgram>` to the creator initialization, the
  recompute-initial-state closure and every participant's `ExecContext`. Same
  for `fixtures.rs`.
- Stability check (a test in `arena0-sandbox`, using the real built guest
  from `programs/target`, not WAT): two engines built by `new_persistent` on
  the same fresh temporary directory in the same process. The first `load`
  records a cache miss and the second a hit (`cache_hits() == 1`). In
  addition, report measured evidence in the result: run
  `cargo nextest run -p arena0-tests --test rock_paper_scissors_bilateral`
  twice in a row and show that the second run compiles nothing (log
  `cache_hits`/`cache_misses` once per process at `debug` level from
  `shared_test_engine` users, or measure `load` time). If guests are not
  byte-reproducible across `just build-programs` runs, the cache key changes
  every build; check this (build twice, compare the `sha256sum` that the build
  already prints) and report it.
- CI: the cache lives inside the target dir, which the CI cache already
  preserves. Check `.github/workflows` and `docs/development.md` and mention
  the cache directory in `docs/development.md` (one sentence).

### No gossip wait for a late subscriber

The negotiation driver emits its facts at start and then only every 2 s ± 50 %
(`CADENCE_MS`). A peer that subscribes after that first emission waits for the
next resend (≈0.9 s measured). On `ProgramTopicEvent::NeighborUp(peer)` in
`machines/negotiation/driver.rs:541`, when the peer is new to `neighbors`, send
the current facts right away (reuse `emit_periodic`, or its targeted
equivalent if one exists). The cadence stays as the loss-recovery resend. Add a
node test: a participant that joins after the creator's first emission
activates without waiting for a cadence tick. Assert it with paused tokio time
or a bound well below the 1 s minimum cadence.

### Verify

`cargo fmt --all`, `just check`, `just test`. Report the RPS, chess,
sequential_count and contract_net Arena test times, each run alone, before
(4.5 s / 4.8 s / 11.0 s / measure) and after. Also report the full `just test`
wall time before and after.

## Step D3b — program scenario tests run on real Hosts; delete the native SDK harness

Owner decision 2026-09-25: option 1. Program scenario tests run through the
existing `arena0-tests` Arena (real Hosts, SQLite, local network, built Wasm).
Depends on D3a.

- Delete `crates/arena0-sdk/src/testing.rs` and `testing/*`,
  `crates/arena0-sdk/tests/fixtures.rs`, the `#[arena0::test]` macro
  (`sdk-macros/src/arena0_test.rs` and its registration), SDK re-exports
  (`lib.rs`, `prelude.rs`) and anything only they used (for example
  `push_effect`/`push_log` shims if production code does not need them, and
  `DivergenceDiagnostic`/`DivergenceKind` in `arena0-protocol` if nothing else
  uses them after this). Delete the `testing` cargo feature or dev-only
  dependencies they needed.
- Program crates (`programs/*`, `examples/minimal-program`): keep plain native
  `#[test]`s that exercise pure program functions and types (rules, scoring,
  move legality, codecs) without a harness. Every test that drove handlers
  through `TestHarness`/`Scenario`/`#[arena0::test]` moves to
  `crates/arena0-tests/tests/<program>_*.rs` as an Arena run. Merge scenarios
  that share a setup into one run (one run can check a rejected input, then
  play on and check the outcome). Assert observable results: outcomes,
  `InputRejected` for bad answers, callout contexts, views and queries,
  receipts. Where a pure function test already proves a rule, do not add a
  Host run for it.
- Extend `Arena`/`Run` only with what the moved tests need, for example:
  expecting that an answer is rejected (the typed `InputRejected` result from
  the real Host), reading the open callout context, a query or view on one
  participant, custom params. Keep the API small and typed.
- Coverage table in the report: every deleted test goes to "native pure test",
  "Arena test <name>" or "dropped: tested only the native harness" (with the
  reason).
- Docs: `docs/programming.md` and `docs/development.md` sections about testing
  programs (the native harness, `#[arena0::test]`, `Scenario`) are rewritten:
  pure logic gets native unit tests, and behaviour gets Arena tests in
  `arena0-tests`, which need `just build-programs` first. Grep docs/, README.md,
  the SDK crate docs and the `cargo-arena0` templates (`cargo arena0 new`, if it
  generates a test using the harness) for `testing::`, `TestHarness`,
  `Scenario`, `arena0::test` and `BilateralPair`.
- `justfile`/`scripts/check-affected.sh`: if program behaviour tests now live in
  `arena0-tests`, make sure the affected-change routing for `programs/*` also
  runs the matching `arena0-tests` targets (read `docs/development.md` for the
  routing contract and update it with the change).

### Verify

`just build-programs`, `cargo fmt --all --check`, `just check`, `just test`.
Report the deleted line count (prod/test), the coverage table, and the new
wall time of `just test` and of the program suite.

## Step D2a — one pass through the dispatch path (protocol, node, sandbox)

Round-1 §3.2, §3.8, §3.9, §3.14 and R2 §3a/§3b as they stand after Stage E.
Behavior does not change: the same events are accepted and rejected with the
same outcomes. Every structural invariant still has one checker.

### Protocol (`execution/state.rs`)

- `validate_recovered` runs only where untrusted bytes enter:
  `ExecutionState::decode` (and the Borsh/serde decode impls that route
  through it). Delete the post-transition `next.validate_recovered()?` calls
  in `activate`, `apply_dispatch`, `drop_outgoing_head`, `add_step_signature`,
  `certify_step`, `stop`, `publish_receipt` (≈9 sites). Each transition
  already checks its own inputs. Where one relied on the full re-check for a
  field it changes, add the narrow check to that transition (name it in the
  report). The re-check re-hashes the full shared image and BLS-verifies the
  last certificate on every transition.
- Protocol tests: add one test helper `assert_valid(&state)` that calls
  `validate_recovered` and use it after each transition in the existing
  transition tests. The tests then prove that the real transitions preserve
  the invariants, without paying for it at runtime.
- `SharedProposal.commitment` duplicates `StepCommitment::for_entry(session,
  &entry, link)`. Delete the field and derive the commitment where it is needed
  (`ExecutionState::proposal_commitment(&self) -> Option<StepCommitment>`, or
  a method taking the session hash and link). Delete the consistency check in
  `validate_recovered` that compared the two. If deriving it in a hot path
  (signature verification per peer frame) costs a measurable amount, cache it
  in memory only (a non-serialized field is not allowed; compute once per
  call instead).
- `ExecutionState` encode clones everything into `ExecutionStateBody`
  (`state.rs:254-341`). Encode without cloning: serialize a borrowing body
  (`struct ExecutionStateBodyRef<'a>` deriving `BorshSerialize` with the same
  field order), or derive directly on `ExecutionState` and keep decode as
  `from_slice` + `validate_recovered`. Pick the smaller diff.
- `apply_dispatch` takes the post-state `StateHash` computed by the sandbox
  (see below) instead of re-hashing `post_shared`. Local-event
  `LocalSharedChange` compares that hash with `agreed_state`.

### Sandbox

- The dispatch result carries `shared_hash: StateHash` (typed, computed once
  by the sandbox from the bytes it returns). A rejected or trapped dispatch
  returns no images (`CallStatus::Rejected` result carries only the reason),
  so nothing is cloned on rejection.
- `ProgramInstance::commit_payloads` returns nothing (`commit(&mut self)`).
  The node no longer compares the committed payloads with a copy of the
  result.
- The four fresh-instance projections (`writer`, `query`, `view`, `outcome`)
  repeat validate, `invoke`, `ensure_read_only`. Fold that into one private
  `fn project<I, O>(&self, kind, export, input) -> Result<(O, u64), _>`
  (returns output and fuel). Keep the per-call result checks.
- Envelope bounds: `call.rs::serialize` checks the constant
  `MAX_CALL_ENVELOPE_BYTES`, while `runtime.rs` checks
  `profile.limits.max_call_envelope_bytes` at four sites. Keep one bound check
  per direction (encode input, decode output) in one helper used by both the
  fresh and the resident paths, against the profile limit (the profile
  owns it). Delete `call.rs::serialize` if it becomes unused.
- `engine/instance.rs:173-234` versus `engine/mod.rs:345-394` duplicate
  metadata instantiation. Share it if the import sets allow it, otherwise say
  why not.

### Node (`execution/guest.rs::dispatch_event`)

- Pre-dispatch checks (callout match, terminal, staged proposal, not Active)
  run before the resident is touched, so they return without
  `discard_candidate`.
- Everything after `resident.dispatch` runs in one inner function. The outer
  function calls `discard_candidate` once on any `Err` or `Rejected` outcome,
  and on `Committed` with a staged proposal (as today). The goal is one
  discard site, not 11.
- `let mut next = self.state.clone()` moves to just before `apply_dispatch`
  (after the sandbox result is accepted), so a rejected dispatch clones
  nothing.
- Delete the node-side `StateHash::of_shared(&result.shared)` recompute and
  the `candidate_shared`/`candidate_local` clones and comparison. The sandbox
  owns the hash (above).
- `reconcile_resident` compares two full images on every dispatch. The actor
  owns both the state and the resident, so track synchronisation explicitly
  (`resident_in_sync: bool`, cleared by every path that changes
  `self.state` images without committing the resident, set by
  restore/commit), or compare the `StateHash` plus a local-image hash if one
  is already at hand. Choose the smaller correct option and explain it.

### Tests / verify

Keep every existing behavioural test green. Add: protocol `assert_valid`
usage as above; a sandbox test that a rejected dispatch leaves the resident
at its committed images and returns no images; a node test that a rejected
input followed by an accepted one commits exactly the accepted result.
`cargo fmt --all`, `just check`, `just test`. Report the RPS Arena test time
alone before and after (D3a numbers are the baseline).

## Step D2c — the store keeps each fact once

Shapes are decided; add no type, function or column not listed. If a shape
cannot be achieved, write `impl/conflict-d2c.md` and stop; do not improvise.


- `terminal_proofs.publication` duplicates `receipts.artifact` for the same
  `receipt_id` (`store/schema.sql:106-125`, `database/receipt.rs:253-415`
  decode and compare both copies). Drop the `publication` column; the row
  keeps `(execution_id, version, receipt_id)` with a foreign key to
  `receipts(receipt_id)`. Readers load the artifact through the receipt row.
  Delete the byte cross-comparison, and keep the relation checks (row count
  against status, execution relation).
- `executions.local_state_checksum` is derived from the local image already
  stored inside `executions.state`. Its only reader
  (`database/execution.rs:313`) compares it back against that image. Delete
  the column, both writes (`insert`, `persist_state`), the read, and the
  comparison. If `checksum` loses its last caller, delete it.
- Keep the SQL projection columns (`version`, `lifecycle`, `agreed_step`,
  `event_position`, `end_phase`, `end_unconfirmed`) and their decode
  cross-checks. They serve SQL filters and compare-and-set; they are not
  in scope.
- Persist path (`database/execution.rs:410-425`): `event_bytes` and
  `effects_bytes` are each called once per dispatch persist today. Leave the
  path alone unless you find a second encoding of the same value within one
  persist; if you do, hoist it into one `let` and list it in the report. Add
  no helper.
- Bump the store schema version once. No migration.
- Tests: reopen after publication loads the artifact through the relation;
  a corrupted relation (receipt row missing) is `Corruption`; delete tests that
  only exercised the duplicate-byte comparison.

## Step D2b — derive codecs; bounded field types

Round-1 §3.12. There are 63 hand-written Borsh impls in `arena0-protocol`, 34
in `arena0-program` (`abi.rs`, `schema.rs`, `state.rs`) and 6 in
`arena0-wire`.

- Introduce bounded field types in one place (`arena0-program`, re-exported
  where needed) whose `BorshDeserialize` enforces the bound while reading, for
  example `BoundedBytes<const MAX: usize>` and `BoundedString<const MAX: usize>`
  (plus a bounded `Vec<T>` if a list needs one). Reuse the existing bounded
  readers (`arena0_protocol::bounded`) as their implementation, and move them
  if that removes a dependency direction problem.
- Replace hand-written impls with `#[derive(BorshSerialize,
  BorshDeserialize)]` wherever the struct's fields can carry their own bounds.
  Keep a hand-written impl only where decoding validates a cross-field
  invariant; then the impl is `derive`d on a private raw struct plus one
  `TryFrom` (one pattern everywhere).
- Byte identity is not required (no compatibility). If an encoding changes,
  bump the owning version once: `ABI_VERSION`/`EXECUTION_PROFILE_VERSION` for
  `arena0-program` ABI types (and rebuild guests, updating the
  `REQUIRED_FUNC_EXPORTS` fixtures only if exports change), trace/receipt/step
  domain for protocol evidence, store schema for stored state, wire for
  frames.
- Tests: an over-limit decode test per bounded type (it fails before
  allocating the oversized value) and one round trip per replaced type
  family. Delete hand-written-codec tests that only pinned the old byte order.
- Report the impl count before and after per crate.
