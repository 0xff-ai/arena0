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
    -- Explicit requests always carry params; a join may omit its preferred
    -- params until the creator's authenticated offer is available.
    params BLOB CHECK (params IS NULL OR length(params) <= 1024),
    admission BLOB NOT NULL,
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
    local_state_checksum BLOB NOT NULL CHECK (length(local_state_checksum) = 32),
    version INTEGER NOT NULL CHECK (version >= 0),
    lifecycle INTEGER NOT NULL CHECK (lifecycle >= 0),
    public_step INTEGER NOT NULL CHECK (public_step >= 0),
    private_next_record INTEGER NOT NULL CHECK (private_next_record >= 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    FOREIGN KEY (execution_id) REFERENCES activation_records(execution_id)
) STRICT;

CREATE TABLE occurrences (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    occurrence_key BLOB NOT NULL,
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    input BLOB NOT NULL,
    committed_version INTEGER NOT NULL CHECK (committed_version >= 0),
    committed_at_ms INTEGER NOT NULL CHECK (committed_at_ms >= 0),
    PRIMARY KEY (execution_id, occurrence_key),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE occurrence_conflicts (
    conflict_id INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    occurrence_key BLOB NOT NULL,
    existing_digest BLOB NOT NULL CHECK (length(existing_digest) = 32),
    incoming_digest BLOB NOT NULL CHECK (length(incoming_digest) = 32),
    incoming_input BLOB NOT NULL,
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE public_commits (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    step INTEGER NOT NULL CHECK (step >= 0),
    version INTEGER NOT NULL CHECK (version >= 0),
    artifact BLOB NOT NULL,
    entry_hash BLOB NOT NULL CHECK (length(entry_hash) = 32),
    PRIMARY KEY (execution_id, step),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE private_commits (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    version INTEGER NOT NULL CHECK (version >= 0),
    artifact BLOB NOT NULL,
    record_digest BLOB NOT NULL CHECK (length(record_digest) = 32),
    PRIMARY KEY (execution_id, sequence),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE terminal_proofs (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    version INTEGER NOT NULL CHECK (version >= 0),
    proof_id BLOB NOT NULL CHECK (length(proof_id) = 32),
    receipt_id BLOB NOT NULL CHECK (length(receipt_id) = 32),
    publication BLOB NOT NULL,
    PRIMARY KEY (execution_id, version),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE receipts (
    -- One immutable artifact row serves both local publications and foreign
    -- imports.  Provenance is derived from the independent fact tables below.
    receipt_id BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 32),
    proof_id BLOB NOT NULL CHECK (length(proof_id) = 32),
    session_id BLOB NOT NULL CHECK (length(session_id) = 32),
    producer BLOB NOT NULL CHECK (length(producer) = 32),
    artifact BLOB NOT NULL,
    stored_at_ms INTEGER NOT NULL CHECK (stored_at_ms >= 0),
    UNIQUE (session_id, producer)
) STRICT;

-- A durable fact that this Host accepted the artifact through receipt import.
-- It intentionally remains present when the same artifact is later produced
-- by a local execution.
CREATE TABLE receipt_imports (
    receipt_id BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 32),
    imported_at_ms INTEGER NOT NULL CHECK (imported_at_ms >= 0),
    FOREIGN KEY (receipt_id) REFERENCES receipts(receipt_id)
) STRICT;

-- The local execution relation for an artifact produced and sealed by this
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

CREATE TABLE inbox (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    inbox_id BLOB NOT NULL CHECK (length(inbox_id) = 32),
    source BLOB NOT NULL CHECK (length(source) = 32),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    frame BLOB NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('accepted', 'applied', 'consumed')),
    accepted_at_ms INTEGER NOT NULL CHECK (accepted_at_ms >= 0),
    applied_version INTEGER CHECK (applied_version IS NULL OR applied_version >= 0),
    consumed_at_ms INTEGER CHECK (consumed_at_ms IS NULL OR consumed_at_ms >= 0),
    PRIMARY KEY (execution_id, inbox_id),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id),
    CHECK ((status = 'accepted' AND applied_version IS NULL AND consumed_at_ms IS NULL)
        OR (status = 'applied' AND applied_version IS NOT NULL AND consumed_at_ms IS NULL)
        OR (status = 'consumed' AND applied_version IS NULL AND consumed_at_ms IS NOT NULL))
) STRICT;

CREATE TABLE inbox_conflicts (
    conflict_id INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    inbox_id BLOB NOT NULL CHECK (length(inbox_id) = 32),
    existing_source BLOB NOT NULL CHECK (length(existing_source) = 32),
    incoming_source BLOB NOT NULL CHECK (length(incoming_source) = 32),
    existing_digest BLOB NOT NULL CHECK (length(existing_digest) = 32),
    incoming_digest BLOB NOT NULL CHECK (length(incoming_digest) = 32),
    incoming_frame BLOB NOT NULL,
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE TABLE outbox (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    outbox_id BLOB NOT NULL CHECK (length(outbox_id) = 32),
    version INTEGER NOT NULL CHECK (version >= 0),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    effect BLOB NOT NULL,
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    status TEXT NOT NULL CHECK (status IN ('pending', 'leased', 'acknowledged')),
    available_at_ms INTEGER NOT NULL CHECK (available_at_ms >= 0),
    lease_id BLOB CHECK (lease_id IS NULL OR length(lease_id) = 32),
    lease_until_ms INTEGER CHECK (lease_until_ms IS NULL OR lease_until_ms >= 0),
    last_error TEXT,
    PRIMARY KEY (execution_id, outbox_id),
    UNIQUE (execution_id, version, ordinal),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id),
    CHECK ((status = 'pending' AND lease_id IS NULL AND lease_until_ms IS NULL)
        OR (status = 'leased' AND lease_id IS NOT NULL AND lease_until_ms IS NOT NULL)
        OR (status = 'acknowledged' AND lease_id IS NULL AND lease_until_ms IS NULL))
) STRICT;

CREATE TABLE active_timers (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    timer_id BLOB NOT NULL CHECK (length(timer_id) = 32),
    deadline_ms INTEGER NOT NULL CHECK (deadline_ms >= 0),
    payload BLOB NOT NULL,
    armed_version INTEGER NOT NULL CHECK (armed_version >= 0),
    PRIMARY KEY (execution_id, timer_id),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id)
) STRICT;

CREATE INDEX outbox_ready
    ON outbox (execution_id, status, available_at_ms, outbox_id);
CREATE INDEX outbox_causal
    ON outbox (execution_id, status, version, ordinal);
CREATE INDEX outbox_leases
    ON outbox (execution_id, status, lease_until_ms);
CREATE INDEX active_timers_due
    ON active_timers (execution_id, deadline_ms, timer_id);
