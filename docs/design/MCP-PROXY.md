# MCP proxy mode: how it is built

The module behind `[[mcp.servers]]`, as built. [MCP-PROXY.md](../MCP-PROXY.md)
is how to use it; this is the layout, the decisions, and what they cost.

## Why this and not wire interception

Claude Desktop's Chat tab is the case that motivates it, and interception is
the wrong instrument for it. Three obstacles, of which `[proxy.intercept]`
solves only the first:

1. getting the bytes — solved; `claude.ai` on the allow list is enough
2. parsing them — a wire family for a private, versionless conversation API
3. finding anything worth eliding — Chat's history is server-held, so a turn
   carries a new message and a conversation id

And a fourth that interception creates rather than solves. The one place bulk
does cross that wire is a large paste, which is **prose the user wrote and
keeps**. Eliding it rewrites their own transcript server-side, permanently,
where another device has nothing to resolve the markers with. Tool output is
machine-facing; a chat message is not.

The bulk a Chat conversation actually carries comes from **MCP tool results**,
and those are machine-facing in exactly the way tool output in a coding agent
is. Compressing them needs no private API, touches nothing a human reads, and
reuses the retrieval machinery that already exists.

Server-held history, which kills interception, helps here: the elided result is
what the server stores, so every later turn pays the reduced price.

## What exists already

The MCP surface serves four tools — `optscale_kompressor_compress`,
`compress_raw`, `retrieve` and `stats` — over two transports, `stdio.rs` for a
runtime that launches the process and `http.rs` for one that connects. The
retrieve half of the loop is therefore done, including the part that is easy to
underestimate: `guidance.rs` teaches a model the marker grammar, without which
recoverable mode silently becomes lossy.

What is missing is that kompressor can be *called* as an MCP server but cannot
*front* one. That is the whole of this proposal.

`AppCore::compress_raw` is the hook, and it already builds its context with
`Context::for_role("tool")` — a tool result is what it was written for.

And `stdio.rs` already notes that it opens the same SQLite files a running
`serve` does, deliberately: WAL makes that safe, so a handle minted by a
stdio-launched proxy resolves through the HTTP proxy and the other way round.
A Desktop-launched proxy and a system-wide `serve` share one fragment store
without being told to.

## Module layout

```
crates/kompressor-mcp/src/proxy/
    mod.rs      the session: one client in, several servers out
    child.rs    launching and supervising a stdio server
    remote.rs   an HTTP/streamable-HTTP server
    merge.rs    tools/list merging, name prefixing, collisions
    route.rs    which server owns a prefixed tool name
```

### The flow

```
client (Claude Desktop Chat)
  │
  ├─ initialize ......... answered locally, capabilities merged with the servers'
  ├─ tools/list ......... fanned out, names prefixed, our retrieve tool appended
  ├─ tools/call
  │     ├─ ours ......... handled locally; retrieve never leaves the process
  │     └─ theirs ....... routed by prefix, forwarded verbatim, and the result's
  │                       text blocks passed through compress_raw on the way back
  └─ notifications ...... forwarded unchanged in both directions
```

Only the response direction of a `tools/call` is touched. Arguments are
forwarded byte for byte: a wrapped server that validates them strictly must see
what the model actually sent.

## Config surface

A list, defaulting to empty, so the mode is off until a server is named.

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/you/work"]

[[mcp.servers]]
name = "wiki"
# url = "https://wiki.internal/mcp"   # refused for now; see "Limits"
compress = false                       # forward this one untouched
```

```rust
pub struct McpServer {
    pub name: String,
    /// Spawned, speaking JSON-RPC on the pipe. Exclusive with `url`.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Connected to over HTTP. Exclusive with `command`, and refused for now:
    /// rmcp's reqwest client transport wants a newer `reqwest` than the proxy's
    /// own client is pinned to, and bumping that for a secondary transport was
    /// not a trade worth making mid-feature.
    pub url: Option<String>,
    /// Whether results from this server are compressed at all.
    pub compress: bool,
}
```

`command` and `url` are exclusive and one is required — a server that is
neither spawned nor dialled is a configuration error worth refusing at startup
rather than discovering on the first call.

## Decisions worth settling first

**The retrieve tool must arrive in the same `tools/list` as the results it
resolves.** This is not a nicety: a client given markers and no retrieval tool
reads `[... omitted [ref:k_…]]` as a statement that data is missing, and
recoverable mode becomes aggressive mode that also pays for storage. Wiring a
base URL without the tool produced exactly that on Desktop's Code tab. Here the
two arrive together by construction, which is the main argument for this shape
over any out-of-band injection.

**Prefix tool names by server.** Two servers exporting `read_file` is ordinary,
and silently shadowing one is the kind of bug that presents as "the agent calls
the wrong file reader sometimes". `filesystem__read_file` routes without
ambiguity. The cost is that a model's learned tool names change, which is worth
stating in the docs rather than discovering.

**Compress text blocks only, and only over `min_compress_chars`.** An MCP
result is a list of content blocks: text, image, resource. Images and binary
resources pass through untouched — there is nothing the engine does to a PNG
except make it bigger. This is also where `compress = false` earns its place:
a server whose results are already small or structurally fragile is better
excluded than tuned around.

**A dying server must not take the session with it.** If a spawned child exits,
its tools answer with a JSON-RPC error and everything else keeps working; the
next call may restart it, bounded, with the reason logged. A supervisor that
kills the whole session because one wrapped server crashed turns a degraded
tool into a dead assistant.

**Tenancy follows the transport, as it already does.** A spawned proxy has no
connection to read a credential from, so the identity is fixed from the
launching environment — `stdio.rs` settled this and the same rule applies.

**Protocol version is negotiated down, not asserted.** `initialize` has to
reconcile what the client offers, what each server accepts, and what we
implement. The lowest common version is the only answer that cannot
misrepresent a server's capabilities to a client.

**One level, enforced.** Every spawned server is told it is one, and a process
carrying that marker ignores its own `[[mcp.servers]]`. Without it a
configuration whose fronted server is this same binary — reading this same
configuration, inherited through `KOMPRESSOR_CONFIG` — recurses without bound.
Measured at 1,796 processes before the machine ran out of forks. Nothing in such
a configuration looks wrong, which is why this is a guard in the code rather
than a warning in the documentation.

## What this does not do

A Chat conversation with no MCP tools in it gets nothing, and someone pasting a
large log into the message box is not helped. That is the correct outcome: it is
their text, and the right place to compress a file they want discussed is the
tool that reads it.

So "Chat tab support" is two features wearing one name. Compressing Chat's
inference requests is near-worthless and carries real downside. Compressing the
tool results Chat consumes is worthwhile, rests on a published protocol with a
version field, and is most of the way built.

## Testing without a client

The dependency on a real MCP client is avoidable, which is most of the value of
keeping routing and merging separate from transport:

- `merge.rs` — prefixing, collisions, and that our retrieve tool survives a
  merge with a server that exports a tool of the same name
- `route.rs` — a prefixed name routes to one server; an unknown prefix is an
  error and never a silent local handle
- `child.rs` — a fake stdio server that echoes, one that exits mid-call, one
  that writes malformed JSON-RPC
- end to end — a client speaking JSON-RPC over a pipe against a fake server
  returning a large text block, asserting that the result comes back with a
  marker and that `retrieve` on that handle returns the original bytes
