# Internal MCP adapter contract

The adapter remains implemented for internal use. It is not registered by
harness setup or advertised in the agent skill, CLI help, or onboarding.

The daemon exposes one MCP Streamable HTTP endpoint with a stable tool catalog.
`hello` accepts a required `user_agent` for a new Participant and returns a
signed access token, public `peer_id`, expiry, and renewal time. It generates
the local Host ID; callers cannot select an existing Host by name. The user
agent must be nonblank, contain no control characters, and fit within 256 UTF-8
bytes. It describes agent software and is operational metadata, never
authenticated identity or protocol evidence.

Every program, execution, and verification call carries a token. The adapter
validates it once and dispatches through the authorized Host service. Program,
execution, and receipt references contain no caller-selected Host. Admission
targets and participant results use public `PeerId`s. MCP does not enumerate
the daemon's Hosts or map other Participants to local Host names. Operator
Unix API and monitor access remains daemon-wide.

Host access uses HS256 JWTs with a dedicated 256-bit daemon signing key at
`$ARENA0_HOME/mcp-signing.key`. Private, crash-safe persistence preserves that
key across restarts; it is independent of Host identity keys. Verification
requires the expected algorithm, token type, issuer, audience, Host ID,
`PeerId`, issuance time, and unexpired deadline. An existing Host's identity
must match before it is exposed. Missing identity data is an error, never a
request to create a replacement. The daemon stores no per-token records.
The optional `ARENA0_MCP_TOKEN` HTTP bearer credential remains a separate
endpoint access gate; it does not grant access to a named Host.

`hello` with a valid token renews access to the same Host while preserving its
identity and user agent. The default lifetime is 24 hours, configurable by
`--mcp-access-token-lifetime-secs` or
`ARENA0_MCP_ACCESS_TOKEN_LIFETIME_SECS`. Expiry and renewal times are UTC Unix
seconds. Renewed credentials do not revoke earlier tokens; those remain valid
until their own deadlines. Expired or invalid tokens cannot renew. There is
no `goodbye`, refresh-token record, or application session table. Access is
checked at request dispatch; an accepted request may finish after expiry.
Token expiry and transport closure do not stop a Host or its executions.

Initial `hello` is not idempotent. Losing its response loses the credential;
another call without a token creates another Host. The MCP adapter does not
infer identity from caller metadata or promise recovery of an unknown token.
It owns no protocol, execution, sandbox, or receipt state. These access rules
restrict MCP dispatch, not other processes with access to the same home and
administrative Unix socket.

## Tool projection

`arena0d` exposes one stateless Streamable HTTP endpoint at `/mcp`.
`hello({user_agent})` creates a Host and returns `{token,peer_id,expires_at,renew_after}`.
Every other tool requires a top-level `token` argument; the daemon validates
it and dispatches only to the named Host. Program, execution, and protocol
session references contain their respective IDs, without a Host selector.
Tokens are credentials and must not be shared between Participants.

`hello({token})` renews an unexpired token for the same Host. The times are UTC
Unix seconds; the default lifetime is 24 hours. Preserve the latest token
across reconnects and renew at `renew_after`, before `expires_at`. Old tokens
remain valid until their own expiry. An expired or invalid token is rejected;
the call never creates a replacement Host. Token expiry and transport closure
do not stop executions. A lost initial `hello` response cannot be recovered
through MCP without its credential; repeating creation allocates another Host.

`ARENA0_MCP_TOKEN`, when configured, still protects the HTTP endpoint with an
independent bearer credential. It does not replace the per-Host token. The
adapter has no `goodbye` operation or server-side token records.

The stable tool set is:

- access: `hello`;
- programs: `list_programs`, `inspect_program`;
- execution: `start_execution`, `get_execution_status`, `list_executions`,
  `view_execution`, `await_execution_event`, `answer_callout`, `query_execution`,
  `stop_execution`;
- evidence: `verify_session`.

Public Participant identity is returned by `hello`. Negotiation
withdrawal and active termination are one lifecycle-aware `stop_execution`
operation. Params updates are unsupported because negotiation terms are
immutable. Trace and raw receipt retrieval remain operator Unix API/CLI
operations rather than agent tools.

`await_execution_event` uses a bounded wait. A `waiting` result means that the
wait elapsed without a callout or terminal event; it does not withdraw or
finish the execution. Call the tool again with the retained execution reference
to renew the wait, including while an open Join is still discovering an offer.

MCP admission uses public Participant peer IDs:

```json
{"mode":"explicit","peers":["<peer-id>"]}
{"mode":"join","target":{"creator":"<creator-peer-id>","negotiation_id":"<negotiation-id>"}}
```

Mode-specific unknown or conflicting fields are rejected before dispatch.
Peer IDs identify protocol Participants; they do not grant access to another
Host's tools, catalog, executions, or receipts.
