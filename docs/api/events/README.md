# Events: local daemon stream

This directory is the reference for the local `events.subscribe` method. It
defines the event vocabulary, filter grammar, frame shape, and delivery
semantics for the local Unix socket API.

## Documents

- [inventory.md](inventory.md) lists the 21 event tags and their payloads.
- [wire.md](wire.md) defines length framing, JSON fields, and filters.
- [semantics.md](semantics.md) defines ordering, backpressure, and redaction.

## Event tags

Tags have two or three dot-separated segments. Each segment matches
`[a-z][a-z0-9_]*`. The catalog has these domains:

| Domain | Count | Scope |
|---|---:|---|
| `host.*` | 2 | Host identity and shutdown |
| `negotiation.*` | 1 | Offer observations before execution |
| `exec.*` | 2 | Execution creation and termination |
| `exec.negotiation.*` | 10 | Negotiation for one execution |
| `exec.session.*` | 5 | The activated session and program JSON |
| `stream.*` | 1 | Subscriber delivery control |

Valid filters name an exact catalog tag or a subtree wildcard. A wildcard is
the final segment in `<domain>.*` or `<domain>.<subdomain>.*`.
`exec.*` selects root execution tags and both execution subdomains.

`host.started` and `stream.lagged` are delivery-control frames. They bypass
`include` and `exclude` filters. An empty `include` selects every catalog tag;
`exclude` always wins.

## Frame contract

`EventFrame` is the JSON object sent after the subscription acknowledgement.
`kind` is the closed tag and `data` contains only the payload for that tag.
Correlation IDs stay in the frame and never repeat in `data`. The `host`
field is a self-contained emission-time `HostInfo` snapshot:
`{"id":"...","peer_id":"...","user_agent":...}`. It is metadata for
the occurrence, not a mutable lookup performed by the consumer.

- `exec_id` is present if and only if `kind` starts with `exec.`.
- `session_id` is present on all `exec.session.*` events and on the activation
  events that have a known session hash.
- `exec.terminated` may carry `session_id` when the session was identified.
- Other Host, offer, and stream-control frames have no correlation IDs.

Fields use `snake_case`. IDs and hashes use lowercase hexadecimal strings.
Program JSON appears only at the explicit guest boundary in callout and
completed-terminal payloads.
