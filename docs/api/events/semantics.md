# Event delivery semantics

`events.subscribe` reports live host occurrences on one local Unix socket
connection. The event bus is bounded, does not replay old events, and never
blocks the host on a subscriber.

## Publication

The daemon has one event junction. A negotiation or execution occurrence is
constructed once, then the junction emits its safe process-local record and
its `EventFrame` projection. `host.started` is synthesized for each
subscription, and `stream.lagged` is synthesized for a subscriber that falls
behind.

The junction emits a frame only after the owning state transition or durable
write reaches its documented boundary. Event frames do not drive execution and
do not replace requests or durable evidence.

## Ordering

Each Host event publisher assigns a monotonically increasing `u64` `seq`.
Consumers order frames from one Host by `(host.id, boot_id, seq)`. Every frame
carries the complete Host metadata observed at emission time, so consumers do
not need to resolve a mutable Host record to interpret it. A new process uses
a new `boot_id` and starts publisher sequences at one. `host.started` is
the synthetic `seq: 0` frame for a subscription. `ts` is a Unix-millisecond
observation time and does not define order.

There is no ordering guarantee across hosts. Concurrent executions share the
publisher sequence but have no additional relative ordering.

Within one execution, the daemon preserves these causal edges:

1. `exec.created` precedes all other events for that `exec_id`.
2. Negotiation events precede `exec.session.started`.
3. `exec.session.callout_answered` precedes the step that consumes the answer.
4. One terminal event, `exec.session.ended` or `exec.terminated`, closes the
   execution.

## Subscription lifecycle

The server registers the broadcast receiver before it writes the
`{"Ok":"Subscribed"}` acknowledgement. It then sends `host.started` and
streams matching queued and future frames.

The stream has no replay, cursor, resume token, or backlog. Read current or
durable state through the ordinary API methods:

| Need | Method |
|---|---|
| Host identity and liveness | `daemon.info` |
| Existing executions | `exec.list` and `exec.status` |
| Prior trace and terminal evidence | `exec.trace`, receipt, and verification methods |

A graceful stop publishes `host.stopped`. A process failure closes the socket
without that frame. Treat an unexpected close as a possible process failure.

## Filters

`EventFilter` contains `include` and `exclude` lists. An empty `include`
selects all catalog tags. Exclusion wins over inclusion. Matching is by event
tag, not by a raw string prefix.

The server rejects bare `*`, bare domains, unknown tags, unknown subdomains,
embedded or middle wildcards, suffix wildcards, empty segments, leading or
trailing dots, uppercase or punctuation, and patterns with more than three
segments. The error names the invalid pattern.

The server does not filter by `exec_id` or `session_id`. Subscribe before
creating an execution when you need its complete lifecycle. Use the frame IDs
to scope events in the client.

## Bounded delivery

The host uses a bounded broadcast channel. A slow subscriber loses the oldest
unread frames for that subscriber. The host continues without waiting.

When the receiver reports a loss, the delivery loop sends `stream.lagged` with
the number of skipped frames and the last skipped sequence. The control frame
bypasses the filter. A write failure closes only that subscriber.

The stream is not a durable queue. A subscriber must treat lag as a signal to
reload state and evidence through the direct API methods.

## Payload safety

Event payloads contain public identity values, protocol IDs, bounded counts,
lifecycle values, state hashes, fuel counts, and host failure descriptions.
They never contain private keys, signatures, invite tokens, opaque program
bytes, or raw Borsh payloads.

The API exposes guest-produced JSON only where the program contract requires
it:

- `exec.session.callout` carries the callout name, prompt, JSON Schema, and
  guest-produced context.
- `exec.session.ended` carries the completed terminal outcome.

Callout answers are acknowledged by the decimal-string `pending_id` and are
never echoed. Preserve that string exactly when submitting an answer. The
event stream does not carry request parameters, receipt references, query
results, view results, or trace entries.

The process-local `arena0_protocol::system_event::SystemEvent` projection is a
host-only, non-Borsh value. It is redacted separately from the API frame and
does not become a guest or persisted format.

## State reconstruction

Consumers can derive the public lifecycle from the event sequence:

| Lifecycle | Evidence |
|---|---|
| `Negotiating` | `exec.created` with `origin: "request"` |
| `Activating` | `exec.created` with `origin: "recovery"` or a prepared event |
| `Active` | `exec.session.started` |
| `Completed` | A completed `exec.session.ended` |
| `Aborted` | An aborted `exec.session.ended` or a program-aborted termination |
| `Failed` | Any other `exec.terminated` |

Pending callouts remain open until their answer or the execution terminal.
The terminal closes any remaining callout. Queue position is reported on
`exec.created`; later queue state comes from `exec.status`.

## Deliberate omissions

The event stream does not expose signing requests, receipt operations, query or
view results, raw parameters, callout answers, or private proof material.
Those values belong to direct request responses, durable receipts, traces, or
verification results. No event is emitted for a getter, pure calculation, or
per-byte operation.

## Required tests

Tests for this contract must cover:

- all 21 event variants and adjacent `kind`/`data` serialization;
- required and optional frame correlations;
- exact tags, subtree wildcards, exclusions, and malformed filters;
- receiver registration before the acknowledgement;
- `host.started` as the first post-ack frame with `seq: 0`;
- sequence gaps caused by filters;
- `stream.lagged` after receiver overrun;
- redaction of raw request, signing, and program-byte fields.
