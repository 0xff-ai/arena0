-- The schema is deliberately concrete.  `state`, `activation`, and every
-- protocol record blob contain a versioned/checksummed store envelope; the
-- scalar columns below are transactionally verified indexes, not authorities.

CREATE TABLE meta (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT;

CREATE TABLE execution_salts (
    execution_id BLOB PRIMARY KEY NOT NULL CHECK (length(execution_id) = 32),
    salt BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
    ,FOREIGN KEY (execution_id) REFERENCES exec_requests(execution_id)
) STRICT;

CREATE TABLE programs (
    program_hash BLOB PRIMARY KEY NOT NULL CHECK (length(program_hash) = 32),
    wasm BLOB NOT NULL CHECK (length(wasm) <= 67108912),
    imported_at_ms INTEGER NOT NULL CHECK (imported_at_ms >= 0),
    removed_at_ms INTEGER CHECK (removed_at_ms IS NULL OR removed_at_ms >= 0)
) STRICT;

-- The local admission root exists before negotiation.  Activation and
-- execution records are descendants of this immutable request identity.
CREATE TABLE exec_requests (
    created_order INTEGER PRIMARY KEY AUTOINCREMENT CHECK (created_order > 0),
    execution_id BLOB NOT NULL UNIQUE CHECK (length(execution_id) = 32),
    program_hash BLOB NOT NULL CHECK (length(program_hash) = 32),
    -- Creator requests always carry params; a join may omit its preferred
    -- params until the creator's authenticated offer is available.
    params BLOB CHECK (params IS NULL OR length(params) <= 1024),
    admission BLOB NOT NULL,
    -- Request creation and bind_join_target write this projection from
    -- admission.negotiation_id(), with the authoritative admission blob.
    negotiation_id BLOB CHECK (negotiation_id IS NULL OR length(negotiation_id) = 32),
    grants BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    failure TEXT CHECK (failure IS NULL OR length(failure) <= 4096),
    FOREIGN KEY (program_hash) REFERENCES programs(program_hash)
) STRICT;

CREATE TABLE activation_records (
    execution_id BLOB PRIMARY KEY NOT NULL CHECK (length(execution_id) = 32),
    session_id BLOB NOT NULL CHECK (length(session_id) = 32),
    status TEXT NOT NULL CHECK (status IN ('prepared', 'committed')),
    prepared_activation BLOB NOT NULL,
    committed_activation BLOB,
    -- prepare_activation writes borsh ActivationFacts; commit_activation
    -- rewrites them from its committed activation in the blob transaction.
    facts BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    UNIQUE (session_id),
    FOREIGN KEY (execution_id) REFERENCES exec_requests(execution_id),
    CHECK ((status = 'prepared' AND committed_activation IS NULL)
        OR (status = 'committed' AND committed_activation IS NOT NULL))
) STRICT;

CREATE TABLE activation_conflicts (
    conflict_id INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    conflict_kind TEXT NOT NULL CHECK (conflict_kind IN ('prepared', 'committed')),
    existing_activation BLOB NOT NULL,
    incoming_activation BLOB NOT NULL,
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    FOREIGN KEY (execution_id) REFERENCES activation_records(execution_id)
) STRICT;

CREATE TABLE executions (
    execution_id BLOB PRIMARY KEY NOT NULL CHECK (length(execution_id) = 32),
    host_id BLOB NOT NULL CHECK (length(host_id) = 32),
    producer BLOB NOT NULL CHECK (length(producer) = 32),
    session_id BLOB NOT NULL CHECK (length(session_id) = 32),
    state BLOB NOT NULL,
    end_phase INTEGER NOT NULL CHECK (end_phase IN (0, 1, 2)),
    end_unconfirmed BLOB NOT NULL,
    -- insert_execution and persist_state derive these indexes from the state
    -- stored in the same transaction, in canonical activation order.
    participant_ids BLOB NOT NULL,
    participants INTEGER NOT NULL CHECK (participants > 0),
    -- callout_id preserves all u64 bits in a signed SQLite INTEGER.
    callout_id INTEGER,
    callout_index INTEGER CHECK (callout_index IS NULL OR callout_index BETWEEN 0 AND 4294967295),
    -- A local observation owned only by this column, like certification time.
    -- State contains no opening time: validation checks presence only.
    -- persist_state keeps it for the same id, resets it for a new id and
    -- clears it when the callout closes; insert_execution initializes it.
    callout_opened_at_ms INTEGER CHECK (callout_opened_at_ms IS NULL OR callout_opened_at_ms >= 0),
    terminal_reason TEXT CHECK (terminal_reason IS NULL OR length(CAST(terminal_reason AS BLOB)) <= 4096),
    outcome_json BLOB CHECK (outcome_json IS NULL OR length(outcome_json) <= 65536),
    -- insert_agreed_step copies its certified_at_ms in the same transaction;
    -- state-only transitions retain it. NULL before step 0.
    last_step_at_ms INTEGER CHECK (last_step_at_ms IS NULL OR last_step_at_ms >= 0),
    version INTEGER NOT NULL CHECK (version >= 0),
    lifecycle INTEGER NOT NULL CHECK (lifecycle >= 0),
    agreed_step INTEGER NOT NULL CHECK (agreed_step >= 0),
    event_position INTEGER NOT NULL CHECK (event_position >= 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    FOREIGN KEY (execution_id) REFERENCES activation_records(execution_id),
    CHECK ((callout_id IS NULL AND callout_index IS NULL AND callout_opened_at_ms IS NULL)
        OR (callout_id IS NOT NULL AND callout_index IS NOT NULL AND callout_opened_at_ms IS NOT NULL))
) STRICT;

-- `resolve` looks session prefixes up by range (design §3.4).
CREATE INDEX executions_session ON executions (session_id);

-- One immutable local dispatch record per event position. The event and
-- effects are opaque program records; shared/local memory ownership remains
-- in the execution aggregate or its pending proposal. Proposal and agreed
-- step rows are the authorities for staged and committed state.
CREATE TABLE event_records (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    event_position INTEGER NOT NULL CHECK (event_position >= 0),
    event BLOB NOT NULL,
    effects BLOB NOT NULL,
    event_digest BLOB NOT NULL CHECK (length(event_digest) = 32),
    PRIMARY KEY (execution_id, event_position),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE agreed_steps (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    step INTEGER NOT NULL CHECK (step >= 0),
    origin_event_position INTEGER NOT NULL CHECK (origin_event_position >= 0),
    version INTEGER NOT NULL CHECK (version >= 0),
    artifact BLOB NOT NULL,
    entry_hash BLOB NOT NULL CHECK (length(entry_hash) = 32),
    -- The local time this Host stored the step. A local observation: it is
    -- not part of `artifact`, so receipts and trace hashes never carry it.
    certified_at_ms INTEGER NOT NULL CHECK (certified_at_ms >= 0),
    post_state BLOB NOT NULL CHECK (length(post_state) = 32),
    PRIMARY KEY (execution_id, step),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

-- Each agreed step's shared post-state, written in the same transaction as
-- its `agreed_steps` row so historical views read it instead of replaying the
-- trace. A `StepState` envelope of the shared state bytes; their state hash is
-- that row's `post_state`, which readers check. A local copy: never part of a
-- receipt or a trace hash.
CREATE TABLE step_states (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    step INTEGER NOT NULL CHECK (step >= 0),
    shared_state BLOB NOT NULL,
    PRIMARY KEY (execution_id, step),
    FOREIGN KEY (execution_id, step) REFERENCES agreed_steps(execution_id, step)
) STRICT;

CREATE TABLE receipts (
    -- One immutable artifact row serves both local publications and foreign
    -- imports.  Provenance is derived from the independent fact tables below.
    receipt_id BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 32),
    session_id BLOB NOT NULL CHECK (length(session_id) = 32),
    kind TEXT NOT NULL CHECK (kind IN ('receipt', 'stop_report')),
    artifact BLOB NOT NULL,
    -- insert_artifact derives these indexes from the authenticated artifact
    -- for both local publication and import, in its insertion transaction.
    program_hash BLOB NOT NULL CHECK (length(program_hash) = 32),
    completed INTEGER NOT NULL CHECK (completed IN (0, 1)),
    stored_at_ms INTEGER NOT NULL CHECK (stored_at_ms >= 0)
) STRICT;

CREATE UNIQUE INDEX receipts_canonical_session ON receipts (session_id) WHERE kind = 'receipt';
CREATE INDEX receipts_session ON receipts (session_id);

-- A durable fact that this Host accepted the artifact through receipt import.
-- It intentionally remains present when the same artifact is later produced
-- by a local execution.
CREATE TABLE receipt_imports (
    receipt_id BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 32),
    imported_at_ms INTEGER NOT NULL CHECK (imported_at_ms >= 0),
    FOREIGN KEY (receipt_id) REFERENCES receipts(receipt_id)
) STRICT;

-- The local execution relation for an artifact produced by this
-- Host.  A receipt and an execution each have at most one such relation.
CREATE TABLE receipt_productions (
    receipt_id BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 32),
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    FOREIGN KEY (receipt_id) REFERENCES receipts(receipt_id),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id),
    UNIQUE (execution_id)
) STRICT;

CREATE INDEX receipt_productions_execution
    ON receipt_productions (execution_id, receipt_id);

CREATE TABLE active_timers (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    timer_id BLOB NOT NULL CHECK (length(timer_id) = 32),
    deadline_ms INTEGER NOT NULL CHECK (deadline_ms >= 0),
    payload BLOB NOT NULL,
    armed_version INTEGER NOT NULL CHECK (armed_version >= 0),
    PRIMARY KEY (execution_id, timer_id),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;


CREATE INDEX agreed_steps_origin
    ON agreed_steps (execution_id, origin_event_position, step);
CREATE INDEX active_timers_due
    ON active_timers (execution_id, deadline_ms, timer_id);
