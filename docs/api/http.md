# Daemon HTTP API

The daemon binds one loopback HTTP server. `daemon.info` returns its base URL
as `http_url`; append `/mcp` for MCP or `/` for the browser UI from
`ARENA0_UI_DIR`, when set. The directory must hold a built arena0-ui,
including a regular `index.html`; an invalid directory prevents startup.
Use a trusted build directory: asset reads follow filesystem symlinks,
including targets outside that directory.
When configured, the `ARENA0_MCP_TOKEN` bearer token guards `/mcp` only; send
`Authorization: Bearer TOKEN`. The other routes are unauthenticated and
reachable only from loopback. There are no Origin or Host checks.

| Route | Request | Response |
|---|---|---|
| `POST /rpc` | `application/json`, a [daemon Request](json-rpc.md) | HTTP 200 with the JSON `Response`, including API errors |
| `GET /events` | No body | SSE, with keepalives; `host` events contain `EventFrame`, `activity` events contain `ActivityFrame` |
| `POST /uploads` | Binary bytes; exactly `application/wasm` or `application/octet-stream` | HTTP 201, `{"upload":"<BLAKE3 hex>","length":123}` |
| `GET /hosts/{host}/blobs/{hash}` | Host name and full blob hash | Binary bytes, `application/octet-stream`, attachment filename equal to the hash; 404 for an unknown Host or blob |
| `/mcp` | MCP Streamable HTTP | MCP service |
| Other `GET` paths | Browser route or asset path | Files from `ARENA0_UI_DIR` when set; unknown client routes serve `index.html` |

`POST /rpc` uses the same request and response types as the Unix socket,
without the socket's length prefix. For example:

```json
{"method":"host.call","params":{"host":"a","request":{"method":"program.import","params":{"source":{"upload":"<64-hex>"}}}}}
```

Malformed JSON and invalid request shapes retain axum's 400/422 responses;
non-JSON requests return 415. API errors have the usual
`{"Err":{"code":"BadRequest","message":"..."}}` body at HTTP 200.
Subscriptions sent to `/rpc` return `BadRequest`: use `GET /events`.

An event connection starts with `host.started` for each currently open Host,
then receives every Host event and daemon activity. Hosts opened later join
the same stream with their own `host.started` snapshot. Slow subscribers
receive synthesized `stream.lagged` frames before the next available frame;
clients must refresh any state whose changes may have been lost.

## File boundaries and upload lifetime

HTTP never accepts daemon-local paths. Both import methods reject
`{"source":{"path":"..."}}`, and `blob.export` is socket-only. Upload bytes
first, then send `{"source":{"upload":"<hash>"}}` to an import method.
`daemon.stop` is also socket-only: HTTP returns `BadRequest` with
`daemon.stop is accepted only on the local socket` and leaves the daemon running.
Program imports require a Wasm upload; an octet-stream upload returns
`BadRequest`. Missing uploads return `NotFound`.

Uploads are limited to 64 MiB (413 above the limit). Other content types,
including media types with parameters, return 415. Upload files live under
the daemon home's `uploads/` directory and are cleared at every daemon
start. An upload can be imported into multiple Hosts until the daemon
restarts. Imports do not consume uploads.

Program imports store their bytes in the selected Host catalog. Blob imports
copy upload bytes into that Host's owned blob directory, independently of
upload lifetime, and enforce the blob limit of 16 MiB. Existing blob content
keeps its original ownership record.

Hashed `/assets/*` files use
`Cache-Control: public, max-age=31536000, immutable`; the index uses
`no-cache`. Static responses carry the existing CSP, no-sniff, no-referrer,
and frame-denial headers. Missing `/assets/*` files return 404. Files are
read on each request; if `index.html` disappears after startup, client-route
fallback returns 404 `text/plain` with `ARENA0_UI_DIR has no index.html`.

When `ARENA0_UI_DIR` is unset or empty, the daemon serves the API only.
`GET /` returns 404 `text/plain` with the body:
`this daemon serves no web UI; start it with ARENA0_UI_DIR set to a built arena0-ui`.
