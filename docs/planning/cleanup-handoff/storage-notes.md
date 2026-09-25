# Storage stage C — design notes (draft, finalize after B6 lands)

C1 plumbing: Store = OwnerLock + Arc<Inner{ conn: std::sync::Mutex<Connection>, execution_claims, host_id }>.
StoreHandle = clone of Arc. Every async method = `self.run(move |db| db.method(args)).await`
(spawn_blocking + mutex). Delete Command enum, QueuedCommand, owner_loop, dispatch_command,
queue/byte budgets, ExecutionWorkingSet/PendingExecution/cache_pending, validation on miss.
Validate persisted aggregates once at open (reconciliation), not per load.

C2 actor-held state: ExecutionActor.state: ExecutionState loaded once at start. Each transition:
clone → protocol transition → store.persist(record{expected_version, next, event_record?,
agreed_step?, timers set/consume, publication?}) → install on Ok; on Err (unknown outcome) reload
from store and rebuild resident. Store commit_* (dispatch, step sig, terminal sig, stop, interrupt,
publish, activate) collapse into one persist that writes what the record carries; version check
as corruption tripwire (UPDATE ... WHERE version = expected; 0 rows → Corruption). No CAS loops
(guest.rs dispatch_event, ensure_*_signature, persist_abort, finalize_receipt, fail_execution).
Delete ApplyOutcome::VersionMismatch/AlreadyApplied/Inbox* variants, load_state() calls.

C3 delivery (no inbox/outbox tables):
- Transport: add ExecDeliveryRejection::NotYet (retryable). Rejected = invalid frame (sender drops
  it, logs); Conflict = equivocation evidence (sender: terminal error as today).
- New frames: StepCertificate { certificate: StepCertificate }, EndCertificate { certificate:
  TerminalCertificate }. Keep StepSignature/End (single signature) and Message/Abort.
- Sender frame projection fn current_frames(state, me) -> Vec<ExecFrame>:
  * last agreed step certificate (keep `last_certificate: Option<StepCertificate>` in
    ExecutionState, set in commit_shared_inner);
  * staged proposal: Message if I am its writer (establishing broadcast / deferred successor from me),
    plus my StepSignature if present;
  * TerminalProof::Pending: my End signature; Certified/Completed: EndCertificate;
  * Stopped/StoppedPublished with Authenticated(occ) where occ.sender == me: Abort.
- Per peer lane: at most one in-flight send per peer, bounded deadline (e.g. 5 s → drop stream,
  retry next tick). In-memory acked set per peer keyed by frame digest; prune to current frames.
- Receiver classification (ack = acknowledge after commit):
  Message: seq < agreed or same commitment staged → ack; seq == agreed & no proposal & valid →
    dispatch (stage) → ack; different proposal staged at seq → Conflict; seq > agreed → NotYet;
    invalid (wrong writer/prestate/id) → Rejected; divergence → fail (ack).
  StepSignature: step < agreed → ack; matches proposal → add → ack; already present → ack;
    no proposal / step ahead → NotYet; mismatched commitment → Rejected.
  StepCertificate: step < agreed → ack; matches proposal → certify (new
    ExecutionState::certify_step(certificate)) → ack; no proposal → NotYet.
  End: terminal pending matches → add → ack; certified/published → ack; not terminal yet → NotYet.
  EndCertificate: pending matches → certify (new status transition) → ack; already → ack; else NotYet.
  Abort: terminal → ack; valid at cursor → stop → ack; ahead → NotYet; behind → ack (stale).
- Terminal: emit finished at publication (emit_published_terminal without has_unsettled_frames gate);
  actor stays until every peer acked final frames, then persist `frames_delivered` (executions
  column) and retire (return from run). Supervisor (daemon exec_manager.rs:657-666) must not stop the
  actor on the finished message. Recovery candidates: not (published && frames_delivered).
- Delete: store inbox.rs, outbox.rs, schema inbox/inbox_conflicts/outbox tables, lib.rs inbox/outbox
  types, node inbox.rs/outbox.rs (replaced by delivery.rs), InflightSend, MAX_INBOX_BATCH.
- Schema version 3 → 4.
