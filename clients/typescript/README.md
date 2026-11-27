# @optscale/kompressor-client (TypeScript)

A thin client for a running Kompressor (`kompressor serve`): one call per
endpoint, `fetch` only, no dependencies. Node 18+, Deno, Bun, browsers.

```ts
import { Kompressor } from "@optscale/kompressor-client";

const k = new Kompressor({ baseUrl: "http://127.0.0.1:8817" }); // or $KOMPRESSOR_URL; apiKey or $KOMPRESSOR_API_KEY
const { messages, stats } = await k.compress(history, { model: "claude-opus-5-5" });

const safe = await k.compressOrPassthrough(history); // never rejects: the input on any error
await k.compressRaw(toolOutput, { contentType: "log" });
await k.retrieve("k_3f9a…", { range: { lines: { start: 1, end: 80 } } });
```

Options are camelCase here and sent in the server's snake_case. Errors reject
with `KompressorError` (`status`, `code`, `requestId`).

`npm test` builds with `tsc` and runs `node --test` against an in-process fake
server.
