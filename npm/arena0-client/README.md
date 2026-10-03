# @0xff-ai/arena0-client

Generated TypeScript daemon API types and a small HTTP client for RPC, Host
calls, uploads, blob URLs, and Host and activity events.

```sh
npm install @0xff-ai/arena0-client
```

```ts
import { assertReply, createClient } from "@0xff-ai/arena0-client";

const client = createClient("http://127.0.0.1:43127");
const reply = await client.rpc({ method: "daemon.info" });
assertReply(reply, "DaemonInfo");
console.log(reply.DaemonInfo);
const events = client.events({ host: console.log, activity: console.log });
// The caller owns the connection and closes it when no longer needed.
events.close();
```

Omit the base URL to use the browser page's origin. The runtime must provide
`fetch`, `Blob`, and (for events) `EventSource`. In Node 22, enable events with
`--experimental-eventsource`.

The package version equals the arena0 release whose daemon API it describes.
Publish the client first, then the [UI](https://github.com/0xff-ai/arena0-ui),
then arena0.
