# Event inventory

The local daemon publishes 21 event tags. Each `EventFrame` contains a
self-contained emission-time `host` snapshot (`id`, `peer_id`, and optional
`user_agent`), `boot_id`, `seq`, `ts`, `kind`, and `data`. Execution events also
contain `exec_id`. Events emitted after the session is identified contain
`session_id`.

Public types live in `crates/arena0-api/src/events.rs`. The daemon emits each
host occurrence once and projects it to the local event stream.

## Host events

| Tag | Data |
|---|---|
| `host.started` | `{version, transport_key, abi_version}` |
| `host.stopped` | `{reason?, uptime_secs}` |

`host.started` is a per-subscription snapshot. Its envelope `host` carries the
snapshot's `peer_id` and optional `user_agent`; those fields are not repeated
in `data`. It follows the `Subscribed` acknowledgement, uses `seq: 0`, and
bypasses the filter. `host.stopped` is a normal shutdown event.

## Offer events

| Tag | Data |
|---|---|
| `negotiation.offer_seen` | `{program_id, negotiation_id, creator, offer_seq}` |

This event reports an offer observed by the local host before it has an
execution record. It has no `exec_id` or `session_id`.

## Execution events

| Tag | Data |
|---|---|
| `exec.created` | `{program_id, negotiation_id?, queue_position?, origin}` |
| `exec.terminated` | `{reason, failed_class?}` |

`origin` is `request` or `recovery`. `failed_class` is `negotiation`,
`host_stopped`, `program_aborted`, `runtime`, or `invalid_guest_output`.

## Negotiation events

| Tag | Data |
|---|---|
| `exec.negotiation.started` | `{target_size}` |
| `exec.negotiation.offer_accepted` | `{creator, offer_seq}` |
| `exec.negotiation.ticket_accepted` | `{participant, ticket_hash, ticket_count, target_size}` |
| `exec.negotiation.peers` | `{lifecycle, peers}` |
| `exec.negotiation.prepared` | `{participants}` |
| `exec.negotiation.resumed` | `{participants}` |
| `exec.negotiation.committed` | `{participants}` |
| `exec.negotiation.retried` | `{attempt, stage, ticket_count, sig_count, target_size}` |
| `exec.negotiation.rejoined` | `{}` |
| `exec.negotiation.timed_out` | `{stage, ticket_count, sig_count, target_size}` |

The activation events `prepared`, `resumed`, and `committed` carry
`session_id`. `stage` is `gossiping` or `prepared`. `lifecycle` uses the
`ExecLifecycle` representation.

## Session events

| Tag | Data |
|---|---|
| `exec.session.started` | `{ensemble}` |
| `exec.session.callout` | `{pending_id, callout_index, name, prompt, schema, context}` |
| `exec.session.callout_answered` | `{pending_id}` |
| `exec.session.step` | `{step, pre_state, post_state, fuel_used, signers, participants}` |
| `exec.session.ended` | `{terminal}` |

`pending_id` values in the callout rows are decimal JSON strings. Consumers
must preserve the exact string when passing one to `exec.submit`.

All session events carry both correlation IDs. `terminal` is either
`{"completed":{"outcome":...}}` or
`{"aborted":{"step":N,"reason":"..."}}`.

## Delivery control

| Tag | Data |
|---|---|
| `stream.lagged` | `{skipped}` |

`stream.lagged` is synthesized for one subscriber after the bounded event bus
drops unread frames. It bypasses the filter. Its `seq` is the last skipped
publisher sequence.
