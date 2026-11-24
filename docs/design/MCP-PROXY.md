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

What was missing is that kompressor could be *called* as an MCP server but
could not *front* one. That is the whole of this module.

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
    child.rs    launching a stdio server, and calling it
    merge.rs    tools/list merging and name prefixing
    route.rs    which server owns a prefixed tool name
```

### The flow

```
client (Claude Desktop Chat)
  │
  ├─ initialize ......... answered locally, with our own capabilities
  ├─ tools/list ......... our four tools first, then each server's, prefixed
  ├─ tools/call
  │     ├─ ours ......... handled locally; retrieve never leaves the process
  │     └─ theirs ....... routed by prefix, forwarded verbatim, and the result's
  │                       text blocks passed through compress_raw on the way back
  └─ notifications ...... not forwarded in either direction
```

Only the response direction of a `tools/call` is touched. Arguments are
forwarded as sent — same keys, same order, same number spelling — because a
wrapped server that validates them strictly must see what the model actually
sent. The call is rebuilt from the de-prefixed name and those arguments, so
anything else on the request (`_meta` included) does not reach the server.

Fronted servers are connected one after another before the client is served,
each with `startup_timeout` (default 60 s) to answer `initialize`. One that
does not is killed and skipped, so it delays the session by at most that and
cannot hold it. Calls to a server once it is up have no timeout of their own.

## Config surface

A list, defaulting to empty, so the mode is off until a server is named.

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/you/work"]

[[mcp.servers]]
name = "wiki"
command = "wiki-mcp"
# url = "https://wiki.internal/mcp"   # skipped for now; see "Limits" in MCP-PROXY.md
compress = false                       # forward this one untouched
```

```rust
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    /// Spawned, speaking JSON-RPC on the pipe. Exclusive with `url`.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Extra environment for the spawned server; the parent's is inherited.
    pub env: BTreeMap<String, String>,
    /// Connected to over HTTP. Exclusive with `command`, and refused for now:
    /// rmcp's reqwest client transport wants a newer `reqwest` than the proxy's
    /// own client is pinned to, and bumping that for a secondary transport was
    /// not a trade worth making mid-feature.
    pub url: Option<String>,
    /// Whether results from this server are compressed at all. Default true.
    pub compress: bool,
    /// To answer `initialize` before it is killed and skipped. Default 60 s.
    pub startup_timeout: Duration,
}
```

`command` and `url` are exclusive and one is required. The configuration loader
does not check that: an entry naming neither, or both, is skipped when the
session connects, and its tools never appear. A misspelt key is refused when
the file loads, as anywhere else in the configuration, and so is a zero
`startup_timeout`. A name already used by an earlier entry is skipped before
anything is spawned, whether or not that earlier entry started, so which server
owns a prefix never depends on which one happened to come up. A name holding
`__` is skipped too: with servers `fs` and `fs__x`, routing `fs__x__read` would
depend on the order of the list.

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
each call to its tools returns a tool error (`isError`, naming the server and
the tool), it lists no tools from then on, and everything else keeps working.
It is not restarted. A supervisor that kills the whole session because one
wrapped server crashed turns a degraded tool into a dead assistant.

**Logs go to stderr.** Stdout is the protocol, so `kompressor mcp serve` logs
to stderr, at `log.level` and in `log.format`. Every skipped entry is a `warn`
naming the server and the reason; every fronted one an `info`. Until this it
logged nothing at all, and a server that failed to start simply was not there.

**Tenancy follows the transport, as it already does.** A spawned proxy has no
connection to read a credential from, so the identity is fixed from the
launching environment — `stdio.rs` settled this and the same rule applies.

**Protocol version is ours, not negotiated across servers.** `initialize` is
answered from our own server info. The client and each fronted server negotiate
separately, so a client can be told about a version a fronted server does not
speak. Taking the lowest common version would close that gap; it is not built.

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
version field, and is built.

## Testing without a client

The dependency on a real MCP client is avoidable, which is most of the value of
keeping routing and merging separate from transport:

- `merge.rs` — prefixing, our tools first, a fronted tool's description left
  alone, and that our retrieve tool survives a merge with a server that exports
  a tool of the same name
- `route.rs` — a prefixed name routes to one server, a tool name containing the
  separator survives, and an unknown prefix is not routed anywhere: it falls
  through to our own tools, which reject a name they do not know
- `child.rs` — each way an entry is refused: no name, a name holding `__`,
  neither or both of `command` and `url`, a `url`, a command that does not
  start, one that is not an MCP server, one that does not answer `initialize`
  in time
- `tests/proxy.rs`, against a fake stdio server written as a shell script — a
  large result comes back with a marker and `retrieve` on that handle returns
  the original bytes; `compress = false` forwards untouched; a small result is
  byte-identical; an image beside text passes through; arguments arrive as
  sent; `args`, `env` and the one-level marker reach the child; a server that
  exits mid-call is a tool error and the session keeps its other tools; an
  unknown prefix is refused locally; a second entry with a name already used
  is skipped, even when the first did not start; a server that never answers
  costs its own tools and not the session. One more fronts the release binary, when
  it has been built
- `kompressor-cli`'s `tests/cli.rs` — `mcp serve` fronts what
  `[[mcp.servers]]` names, ignores it when started as a fronted server, and
  logs an entry it could not front to stderr while stdout stays protocol

Malformed JSON-RPC from a fronted server is not tested.
