# Event wire format

The Unix socket carries a four-byte big-endian length followed by one JSON
object. The object must satisfy `MAX_FRAME_BYTES` in
`crates/arena0-api/src/frame.rs`.

`events.subscribe` is the only streaming method. The server writes one
`Subscribed` acknowledgement, one `host.started` snapshot, and then
`EventFrame` objects until the client disconnects.

## Frame

`EventFrame` uses an adjacent `kind` and `data` tag inside the envelope:

```json
{
  "host": {
    "id": "host-01",
    "peer_id": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "user_agent": "arena0d/0.6"
  },
  "boot_id": "4c7a91f2b8e64d20a5c3f1e89b6d2a47",
  "seq": 12,
  "ts": 1788022419880,
  "kind": "exec.session.step",
  "exec_id": "259ee2c93c7c415069dcf3822ec3ed8e4bb18629ae01d304f17bc66d24f82a40",
  "session_id": "8ddcaf9240edd0067f266b6b56c23caf795995321a86fb2a0e8e8053f20740d6",
  "data": {
    "step": 1,
    "pre_state": "c4a7d974c56f03351afe0341e290799a40bbe94c8fcbb5a2e15a7d7cbcfe75b2",
    "post_state": "8b2f49023de586beeceeeaaef110d5d777cfdea0fa0a9d24365afcd2cb2a4c4",
    "fuel_used": 124,
    "signers": 2,
    "participants": 2
  }
}
```

- `host` is the self-contained emission-time HostInfo snapshot for the Host
  that publishes the frame (`id`, `peer_id`, and optional `user_agent`).
- `boot_id` identifies one process instance.
- `seq` is the publisher sequence. It starts at one for publisher events.
- `ts` is the Unix-millisecond observation time.
- `kind` is one closed tag from [the inventory](inventory.md).
- `data` contains exactly the payload fields for `kind`.
- `exec_id` is present on every `exec.*` tag.
- `session_id` is present when the session hash is known for that occurrence.

Correlation IDs appear only in the envelope. Optional payload fields are
omitted instead of serialized as `null`.

The two synthetic frame rules are fixed:

- `host.started` follows the acknowledgement and uses `seq: 0`.
- `stream.lagged` uses the last skipped publisher sequence.

## Subscription request

The existing filter is carried inside an explicitly targeted Host request:

```json
{
  "method": "host.call",
  "params": {
    "host": "host-01",
    "request": {
      "method": "events.subscribe",
      "params": {"filter": {"include": ["exec.*"], "exclude": []}}
    }
  }
}
```

The acknowledgement is a separate JSON response:

```json
{"Ok":"Subscribed"}
```

## Filter grammar

`include` and `exclude` contain exact tags or subtree wildcards. A tag has two
or three dot-separated segments. Each non-wildcard segment matches
`[a-z][a-z0-9_]*`.

Valid examples include:

- `exec.created`;
- `exec.*`, which selects root execution and both execution subdomains;
- `exec.session.*`, which selects the session subdomain.

The trailing `*` is the only wildcard position. The server rejects bare `*`,
bare domains, unknown tags, unknown domains or subdomains, embedded or middle
wildcards, suffix wildcards, empty segments, leading or trailing dots,
uppercase or punctuation, and four-or-more segments.

An empty or absent `include` selects all catalog tags. An empty or absent
`exclude` excludes nothing. Exclusion wins. `host.started` and
`stream.lagged` bypass both lists.

## Example frames

### `host.started`

```json
{
  "host": {
    "id": "host-01",
    "peer_id": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "user_agent": "arena0d/0.6"
  },
  "boot_id": "4c7a91f2b8e64d20a5c3f1e89b6d2a47",
  "seq": 0,
  "ts": 1788022390012,
  "kind": "host.started",
  "data": {
    "version": "0.6.0",
    "transport_key": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "abi_version": 20
  }
}
```

### `negotiation.offer_seen`

```json
{
  "host": {
    "id": "host-01",
    "peer_id": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "user_agent": "arena0d/0.6"
  },
  "boot_id": "4c7a91f2b8e64d20a5c3f1e89b6d2a47",
  "seq": 1,
  "ts": 1788022390990,
  "kind": "negotiation.offer_seen",
  "data": {
    "program_id": "1136abbc9b158ddaa63a7166e6139dba6dd2f705d1887bbaac9a993b38837575",
    "negotiation_id": "8a12151daee177b7bd42d31a1629edd1b97cf8538103a0aa5f5d7881277fdd27",
    "creator": "1f8b1ec94ba6344314da85051f09f2a22d3bb93e21ed836d69df0b5d31724322",
    "offer_seq": 1
  }
}
```

### `exec.created`

```json
{
  "host": {
    "id": "host-01",
    "peer_id": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "user_agent": "arena0d/0.6"
  },
  "boot_id": "4c7a91f2b8e64d20a5c3f1e89b6d2a47",
  "seq": 2,
  "ts": 1788022391200,
  "kind": "exec.created",
  "exec_id": "259ee2c93c7c415069dcf3822ec3ed8e4bb18629ae01d304f17bc66d24f82a40",
  "data": {
    "program_id": "1136abbc9b158ddaa63a7166e6139dba6dd2f705d1887bbaac9a993b38837575",
    "negotiation_id": "8a12151daee177b7bd42d31a1629edd1b97cf8538103a0aa5f5d7881277fdd27",
    "origin": "request"
  }
}
```

### `exec.session.ended`

```json
{
  "host": {
    "id": "host-01",
    "peer_id": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "user_agent": "arena0d/0.6"
  },
  "boot_id": "4c7a91f2b8e64d20a5c3f1e89b6d2a47",
  "seq": 3,
  "ts": 1788022391700,
  "kind": "exec.session.ended",
  "exec_id": "259ee2c93c7c415069dcf3822ec3ed8e4bb18629ae01d304f17bc66d24f82a40",
  "session_id": "8ddcaf9240edd0067f266b6b56c23caf795995321a86fb2a0e8e8053f20740d6",
  "data": {
    "terminal": {
      "completed": {
        "outcome": {"winner": "Rock"}
      }
    }
  }
}
```

### `stream.lagged`

```json
{
  "host": {
    "id": "host-01",
    "peer_id": "a2f28686c4b4328ae4ce1c3a924d7522e2c08da862e3c2e7af5c912505eaea9a",
    "user_agent": "arena0d/0.6"
  },
  "boot_id": "4c7a91f2b8e64d20a5c3f1e89b6d2a47",
  "seq": 20,
  "ts": 1788022419999,
  "kind": "stream.lagged",
  "data": {"skipped": 4}
}
```
