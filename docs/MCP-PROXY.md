# Compressing MCP tool results

How to put Kompressor in front of the MCP servers a client already uses, so
their results are compressed before they reach the model.

This is the integration for clients that cannot be pointed at the HTTP proxy —
Claude Desktop's **Chat tab** among them. Their inference requests carry no bulk
worth eliding, because the conversation lives on the server; what they do carry
in bulk is tool results, and those are machine-facing in exactly the way tool
output in a coding agent is.

For the Code tab, use [CLAUDE-DESKTOP.md](CLAUDE-DESKTOP.md) instead — it
compresses the whole request, which is strictly more. How the module is built,
and why, is in [design/MCP-PROXY.md](design/MCP-PROXY.md).

## Contents

- [What it does](#what-it-does)
- [1. Install the binary](#1-install-the-binary)
- [2. Write a configuration](#2-write-a-configuration)
- [3. Prove it on the command line](#3-prove-it-on-the-command-line)
- [4. Register it with Claude Desktop](#4-register-it-with-claude-desktop)
- [5. Verify from Chat](#5-verify-from-chat)
- [When it does not work](#when-it-does-not-work)
- [Limits](#limits)

## What it does

![Compressing the Chat tab](charts/mcp-proxy.drawio.png)

For what this looks like with nothing in front of it, see
[the baseline](charts/desktop-baseline.drawio.png) — the Chat half of it is the
argument for this mode.

The chart is a `.drawio.png`: the picture and its source are the same file, so
opening it in drawio edits both. Re-export with `-e` or the source is lost.

```text
Claude Desktop Chat
  └─ kompressor mcp serve            registered as an MCP server
       ├─ our tools ................ compress, compress_raw, retrieve, stats
       └─ fronted servers .......... their tools, prefixed, results compressed
```

A `tools/call` for a fronted tool is forwarded with its arguments verbatim, and
its text results are passed through the engine on the way back. Large results come back with a
marker, and the model resolves it with `optscale_kompressor_retrieve`, which is
listed beside them.

Arguments are never rewritten: a server that validates them strictly sees what
the model actually sent.

Server-held conversation history works in favour of this. The elided result is
what the client's server stores, so every later turn pays the reduced price.

## 1. Install the binary

Proxy mode is experimental and built only from the `feat/mcp-proxy` branch,
version 2.1 and later (`kompressor version` prints it); the 2.0 builds and the
1.x packages do not have it. `kompressor mcp serve` itself is older than proxy
mode, so its presence proves nothing. Ask the binary whether it knows the key,
once the configuration from step 2 exists:

```bash
kompressor check-config --config ~/.config/kompressor/mcp-proxy.toml >/dev/null && echo "proxy mode known"
```

Install the build that has it:

```bash
cp target/release/kompressor ~/.local/bin/kompressor
```

A stale binary fails loudly rather than quietly, which is a mercy: it does not
know `[[mcp.servers]]`, and the configuration is rejected whole, exit status 2.

```text
kompressor: could not parse config: TOML parse error at line N, column 7
  |
N | [[mcp.servers]]
  |       ^^^^^^^
unknown field `servers`, expected one of `enabled`, `path`, `allowed_hosts`, …
```

Claude Desktop shows that as a server which failed to start, with the reason
buried. Check the binary first and the message is plain.

## 2. Write a configuration

A dedicated file, so the MCP proxy can be changed without touching a running
`serve`:

```toml
# ~/.config/kompressor/mcp-proxy.toml
[engine]
mode = "recoverable"

[refs]
backend = "sqlite"
path = "/home/YOUR_USER/.local/share/kompressor/refs.db"

[stats]
enabled = true
path = "/home/YOUR_USER/.local/share/kompressor/stats.db"

[[mcp.servers]]
name = "fs"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/YOUR_USER/work"]
compress = true
startup_timeout = "60s"   # to answer initialize; the default
```

Each `name` must be unique and must not contain `__`, the separator between a
server's name and its tools'. A later entry reusing a name is skipped.

`recoverable` matters: in any other mode an elision retains nothing and the
marker carries no handle, so there is nothing to fetch back.

**Point `refs.path` at the same file a running `serve` uses.** That is
deliberate and safe — WAL makes concurrent readers and one writer work across
processes — and it means a handle minted here resolves through the HTTP proxy
and the other way round.

**Front something harmless first.** A server whose configuration carries a
credential ends up with that credential in a second file; prove the mechanism
with something dull, then decide whether to move the rest.

## 3. Prove it on the command line

Before involving the GUI, which fails slowly and says little. This drives the
binary exactly as Desktop will:

```bash
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | KOMPRESSOR_CONFIG=~/.config/kompressor/mcp-proxy.toml \
    kompressor mcp serve 2>/dev/null \
  | grep -o '"name":"[^"]*"' | sort -u
```

Expect our four tools unprefixed and the fronted server's under its name,
sorted, beside the server's own name from `initialize`:

```text
"name":"fs__read_text_file"
…
"name":"optscale_kompressor"
"name":"optscale_kompressor_compress"
"name":"optscale_kompressor_compress_raw"
"name":"optscale_kompressor_retrieve"
"name":"optscale_kompressor_stats"
```

Only our four and nothing prefixed means no server was fronted. Drop the
`2>/dev/null` and the reason is on stderr: `mcp serve` logs there, because
stdout is the protocol. Each entry that was skipped has a `mcp server not
fronted` line naming it and saying why. No such line, and nothing prefixed,
means `KOMPRESSOR_MCP_FRONTED` is set in the environment that launched it.

Claude Desktop keeps that stderr too, in its log for the server
(`mcp-server-kompressor.log` beside its other logs).

## 4. Register it with Claude Desktop

In `~/.config/Claude/claude_desktop_config.json`, inside `mcpServers`. Keep the
other entries; JSON does not expand `~` or `$HOME`.

```json
{
  "mcpServers": {
    "kompressor": {
      "command": "/home/YOUR_USER/.local/bin/kompressor",
      "args": ["mcp", "serve"],
      "env": {
        "KOMPRESSOR_CONFIG": "/home/YOUR_USER/.config/kompressor/mcp-proxy.toml"
      }
    }
  }
}
```

If a server listed in `[[mcp.servers]]` is already registered here directly,
**remove that entry**. Leaving both means the client sees the same tools twice,
once compressed and once not, and which one a model reaches for is a coin toss.

Then **fully quit and reopen Desktop**. A reopened window is not enough: the
server is spawned per session with the environment it was given.

A fronted server inherits that environment, with one exception: the credential
`kompressor mcp serve` itself runs with (`KOMPRESSOR_API_KEY`,
`KOMPRESSOR_SECRET`, `KOMPRESSOR_TENANT`) is not passed on. A server that needs
one of those — a second kompressor, fronted on purpose — gets it from the `env`
table of its own `[[mcp.servers]]` entry.

## 5. Verify from Chat

Ask Chat to use a fronted tool on something bulky — reading a large file is the
easy case. Then, outside:

```bash
sqlite3 ~/.local/share/kompressor/refs.db \
  'select count(*) || " fragments, newest " || datetime(max(created_at), "unixepoch", "localtime") from refs;'
```

```bash
sqlite3 ~/.local/share/kompressor/stats.db \
  'select surface, outcome, tokens_in, tokens_out, refs_emitted from optimization_events order by id desc limit 5;'
```

Rows with `surface = mcp` and a non-zero `refs_emitted` mean a result was
elided and a handle minted. If the model then answered a question about the
elided part, the loop closed.

A fold of *identical* lines — `connection refused [×380]` — carries no handle
and needs none: the count preserves everything. That is a compressed result, not
a failed one.

**Ask the database, not the model.** The rows above are the evidence; the
model's account of its own context is not. Through the HTTP proxy that is
because handles are resolved before the content reaches the model, so it
reports nothing compressed even when thousands of tokens were removed.

This surface differs: the marker reaches the model unresolved, and a model that
calls `optscale_kompressor_retrieve` itself *is* a witness, because the tool
call appears in the transcript. That is the one place asking is meaningful —
and what to ask is "did you call retrieve", not "do you see markers".

## When it does not work

| symptom | cause |
|---|---|
| the server fails to start, `TOML parse error` near `[[mcp.servers]]` | stale binary; it does not know the key |
| only our four tools, nothing prefixed | a `command` that is not on PATH for the spawning process, one that exits immediately, an entry with no `command` (or a `url`), or `KOMPRESSOR_MCP_FRONTED` set in the launching environment |
| the session is slow to start, then one server's tools are missing | that server did not answer `initialize` within its `startup_timeout` (default 60 s) and was killed; raise it for a slow first `npx` download |
| one of two entries' tools missing | both have the same `name`; the later one is skipped |
| `mcp server … could not run …` in a result | the fronted server failed; its other tools and ours keep working by design |
| a marker with no handle | a lossless fold, or `mode` is not `recoverable` |
| `no such handle is held for this tenant` | `refs.path` differs between the process that minted and the one asked, or the mode is wrong |
| the same tool twice in Chat | the fronted server is also registered directly in `claude_desktop_config.json` |
| tools vanish after an upgrade | a fronted server's `command` moved; the session logs it to stderr, skips it, and continues |

## Limits

**Stdio only.** A `url` upstream is skipped, like any entry that cannot be
fronted, with `url is not supported yet; use command` on stderr. Fronting
over HTTP needs rmcp's reqwest client transport, which wants `reqwest` 0.13.2 or
newer, and the proxy's own HTTP client is pinned to 0.12.

**The HTTP MCP surface does not front servers.** `serve --mcp` offers our four
tools as before; proxy mode is `kompressor mcp serve`, the stdio server a client
launches.

**One level.** A server this process spawns is told so, and ignores its own
`[[mcp.servers]]`. Without that, a configuration whose fronted server is this
same binary reading this same configuration recurses until the machine runs out
of processes — which is not hypothetical, and is why the guard exists.

**Nothing a person wrote is touched.** A Chat conversation with no tool calls in
it gets nothing, and a large paste into the message box is not compressed. That
is the intended outcome: the place to compress a file someone wants discussed is
the tool that reads it.
