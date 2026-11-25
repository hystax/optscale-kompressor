# Cursor support: design

**Status: built; the live tiers are open.** The code of Tiers 1, 2, 4, 5 and 7
is in the tree (`crates/kompressor-cli/src/hook/`, `wrap/hooks.rs`, schema v20,
the dashboard's Cursor page), with the user guide in
[docs/CURSOR.md](../CURSOR.md). What is *not* done is everything marked
**[live]**: Tier 0's re-test on current builds, Tier 3's week of shadow
measurement (Gate A), Tier 6's A/B on Cursor's own accounting (Gate B), and the
live acceptance criteria of the other tiers. Those cost plan usage and are run
by hand. Until they are, the defaults stay where the plan puts a feature
awaiting its gate: MCP compression on once installed, shell rewriting **off**
and, when switched on, offered only when the CLI would approve the rewrite
(Q1, answered in §12.6),
and no saving claimed anywhere. §13.12 lists what was built against each tier.

The mechanisms were exercised against a real Cursor CLI and desktop app on
2026-11-25 with throwaway hooks — §12 has the results. It is the design for
supporting the Cursor agent on both of its surfaces — the console
(`cursor-agent`) and the desktop editor — and it replaces the two catalogue
entries that said Cursor could not be compressed.

Measured against, on Linux x86_64, 2026-11-25:

| What | Version |
|---|---|
| Kompressor | 2.6.0 |
| Cursor CLI (`cursor-agent`, prints itself as `agent`) | 2026.09.26-dd393fe |
| Cursor desktop | 3.28.7 |

The installed CLI build is two months old; the forum threads cited below
discuss 2026.11.05. Repeating the tests on the current build is the first
tier of the plan (§13.2).

Every claim below is tagged with how it is known:

- **[measured]** — read out of the installed build or run on this machine;
  Appendix A has the commands.
- **[docs]** — stated by Cursor's published documentation; see §15.
- **[forum]** — stated by Cursor staff on the Cursor forum, or reported there
  by users; §15 says which.
- **[spike]** — the result of an experiment run on 2026-11-25: thirteen
  sessions on the Linux CLI under `--print` and three in the desktop Agent
  panel. §12 has the results and Appendix A the method. Nothing has been run
  on macOS or on a newer build.
- **[unverified]** — an assumption this design leans on. Each one has an open
  test in §12.5, and §13 says which tier it blocks.

## Contents

1. [Summary](#1-summary)
2. [Where things stand today](#2-where-things-stand-today)
3. [What Cursor offers instead](#3-what-cursor-offers-instead)
4. [Goals and non-goals](#4-goals-and-non-goals)
5. [The design in one picture](#5-the-design-in-one-picture)
6. [Components](#6-components)
7. [Wiring: console and desktop](#7-wiring-console-and-desktop)
8. [Invariants and failure policy](#8-invariants-and-failure-policy)
9. [What this saves, and what it cannot measure](#9-what-this-saves-and-what-it-cannot-measure)
10. [Security and privacy](#10-security-and-privacy)
11. [Alternatives considered](#11-alternatives-considered)
12. [Test results](#12-test-results)
13. [Implementation plan](#13-implementation-plan)
14. [Changes to existing code and docs](#14-changes-to-existing-code-and-docs)
15. [References](#15-references)
- [Appendix A: how the measurements were taken](#appendix-a-how-the-measurements-were-taken)
- [Appendix B: hook payloads](#appendix-b-hook-payloads)

## 1. Summary

Kompressor's proxy cannot serve Cursor, and no amount of work on the proxy
changes that: the agent's traffic is Connect RPC carrying protobuf to Cursor's
own backend, the prompt is assembled on Cursor's servers, and the client never
sends a message array. That finding stands
([DESIGN.md §14.1](../DESIGN.md#141-wrap-running-an-agent-behind-the-proxy)).

What has changed is that Cursor now exposes the same thing the proxy was after —
tool output, before the model sees it — at a documented boundary on the client:
**hooks**. A hook is a command Cursor runs with a JSON document on standard
input and reads a JSON document back from. Two of them can change what reaches
the model:

| Tool output | Hook | What the hook may return | Result |
|---|---|---|---|
| MCP tool results | `postToolUse` | `updated_mcp_tool_output` | the compressed result **replaces** the original |
| Shell command output | `preToolUse` | `updated_input.command` | the command is **rewritten** so its output is compressed before Cursor captures it |
| MCP results over ~40 KB | — | the hook is handed an empty string | spilled to a file by Cursor first; **fronting** the server compresses them (§6.2) |
| Built-in `Read`, `Grep`, `List`, `Fetch` | — | nothing that alters output | **not compressible**; measured only |

Both mechanisms were exercised against the installed CLI on 2026-11-25 and
work (§12). The same experiments found that Cursor already writes any tool
result over 40 KB to a file instead of sending it to the model (§3.5), which
bounds the saving, and that command approval is evaluated on the rewritten
command (§6.3), which dictated the shell form. One session in the desktop
Agent panel confirmed the hooks and the shell rewrite there too, and that a
`postToolUse` hook is handed an MCP result in the same shape as on the CLI.

So the design is a fourth integration style beside *proxy*, *intercept* and
*MCP fronting*: **hook mode**. One new subcommand, `kompressor hook cursor`,
answers Cursor's hook calls with the existing engine and the existing fragment
store. Retrieval is the existing MCP tool server, registered beside the hooks,
with `kompressor retrieve` in the shell as the fallback.

The CLI and the desktop app run the same hook engine and read the same files
**[measured]**, so one implementation covers both. They differ only in how the
hooks are offered:

| Surface | Command | Lifetime | Residue |
|---|---|---|---|
| Console | `kompressor wrap cursor-agent` | one session, via `--plugin-dir` | none |
| Desktop (and any CLI session not started by `wrap`) | `kompressor hook install cursor` | until uninstalled | two marker-owned entries in `~/.cursor/` |

No proxy runs in either case. Hook mode needs the binary, the configuration and
`refs.db`; `kompressor serve` is optional.

## 2. Where things stand today

Two entries in `crates/kompressor-cli/src/wrap/agents.rs`:

| Entry | Wiring | What it does |
|---|---|---|
| `cursor-agent` | `Wiring::Unsupported` | refuses, explains the protobuf finding, points at `wrap cursor` |
| `cursor` | `Wiring::Manual` | starts the proxy and prints the OpenAI base-URL override to paste into Settings; requires the user's own OpenAI key |

The second is documented as unverified end to end: "No Cursor request has ever
reached this proxy" (DESIGN.md §14.1). The reason is now known, and it is not
the empty key field: Cursor's backend makes the call to an overridden base URL,
not the editor, so a loopback address is never reachable **[docs] [forum]**.
§11.2 has the detail and what to do with the entry.

Three properties of Cursor make the proxy the wrong instrument, and they are
the same three that made it wrong for Claude Desktop's Chat tab
([design/MCP-PROXY.md](MCP-PROXY.md)):

1. **The wire is private.** `agent.v1.AgentService` over Connect, protobuf
   bodies, no published schema, a new build every few days **[measured]**: two
   CLI builds three days apart sit side by side in
   `~/.local/share/cursor-agent/versions/`.
2. **History is server-held.** The client streams tool results up as they
   happen; it does not re-send a conversation each turn. There is no message
   array to walk and no frozen prefix to compute.
3. **The bill is not visible.** Usage comes back in Cursor's own accounting, not
   as provider `usage` blocks, so nothing on the wire says what a turn cost.

The second property is also the opening. A tool result enters the conversation
exactly once, from the client. Compress it at that moment and every later turn
pays the reduced price, with no cache to break — the result never changes again.

## 3. What Cursor offers instead

### 3.1 Hooks

A hook is an entry in a `hooks.json`. Cursor runs its `command`, writes the
event as JSON to standard input, and parses standard output as JSON.

**Where hooks are read from** **[measured]**, in priority order **[docs]**.
Every matching hook from every source runs; `deny` beats `ask` beats `allow`,
messages are concatenated, and for any other field — `updated_input` and
`updated_mcp_tool_output` among them — **the last response wins**:

| Source | Path |
|---|---|
| enterprise | `/etc/cursor/hooks.json` · `/Library/Application Support/Cursor/hooks.json` · `C:\ProgramData\Cursor\hooks.json` |
| team | delivered by Cursor's backend to managed accounts |
| project | `<workspace>/.cursor/hooks.json`, trusted workspaces only |
| user | `~/.cursor/hooks.json` |
| plugin | `hooks/hooks.json` inside a plugin, or the manifest's `hooks` key; command hooks only |
| Claude Code compatibility | `~/.claude/settings.json`, `<workspace>/.claude/settings.json`, `.claude/settings.local.json` |

**Events present in both builds** **[measured]**: `sessionStart`,
`beforeSubmitPrompt`, `preToolUse`, `postToolUse`, `postToolUseFailure`,
`beforeShellExecution`, `afterShellExecution`, `beforeMCPExecution`,
`afterMCPExecution`, `beforeReadFile`, `afterFileEdit`, `subagentStop`, `stop`.
The documentation lists more — `sessionEnd`, `subagentStart`, `preCompact`,
`afterAgentResponse`, `afterAgentThought`, the Tab hooks, `workspaceOpen` —
none of which this design uses.

**What a hook can change** **[measured]**, and the documentation agrees
**[docs]** — this table is the whole basis of the design, so each row was read
out of the executor code as well as the hooks page:

| Hook | Field returned | Effect |
|---|---|---|
| `postToolUse` | `updated_mcp_tool_output` | Replaces an **MCP** tool's result. A string becomes one text block; `{"content":[{"type":"text","text":…}]}` becomes one block per item. Honoured in the MCP executor only — no other tool reads it. |
| `preToolUse` | `updated_input` | Rewrites the tool's input, per tool: Shell `command`, `cwd` · Read, List, Delete, ReadLints `file_path` · Grep `pattern`, `file_path` · Fetch `url` · Write `file_path` only (not the content) · MCP tools: keys merged into the arguments. |
| `preToolUse`, `postToolUse`, `postToolUseFailure`, `sessionStart` | `additional_context` | Text added to the model's context. Size-capped: an oversized value is dropped with a warning. |
| `preToolUse`, `before*` | `permission`, `user_message`, `agent_message` | allow / deny / ask. |

There is **no** `updated_tool_output`, no `updated_output`, and nothing on
`afterShellExecution` or `beforeReadFile` that alters what the model receives
**[measured: the strings do not occur in either build]**. Cursor staff describe
the `after*` hooks as intentionally observation-only **[forum]**. Shell output
and file reads cannot be replaced after the fact. That single absence shapes
§6.3.

**Failure is open by default** **[measured]**: a hook that errors or times out
is logged and ignored, unless its entry sets `failClosed: true`. This design
never sets it. Two exceptions matter **[docs]**: exit status 2 blocks the
action, and for a hook that can answer with a permission — `preToolUse` is one
— exit 0 with output that is not valid JSON also blocks it. The handler in §6.1
is written around both.

**What a `postToolUse` hook is given** **[measured]**: the common fields
(`conversation_id`, `session_id`, `generation_id`, `model`, `hook_event_name`,
`cursor_version`, `workspace_roots`, `transcript_path`), plus `tool_name`,
`tool_input`, `tool_output`, `duration`, `tool_use_id`. It fires for every
tool, not only MCP — which is what makes the shadow measurement in §13 possible.

Three details of that payload, from the documentation and a staff thread, that
the handler depends on:

- `tool_output` is a **JSON-encoded string**, not raw text — for a shell call,
  `"{\"exitCode\":0,\"stdout\":\"…\"}"` **[docs]**.
- An MCP tool's `tool_name` is `MCP:<tool_name>`, and the payload carries **no
  server identity** — `mcp_server_name` exists on `afterMCPExecution` only
  **[forum]**.
- `matcher` on these events is a regular expression over `tool_name`: `Shell`,
  `Read`, `Write`, `Grep`, `Delete`, `Task`, `MCP:<tool_name>` **[docs]**.

### 3.2 MCP

`~/.cursor/mcp.json` and `<workspace>/.cursor/mcp.json`, shared by the CLI and
the desktop app; stdio servers are `{"command", "args", "env"}` under
`mcpServers`. A plugin can carry its own `mcp.json` **[measured]**: the cached
`gmail` plugin's manifest is `"mcpServers": "./mcp.json"`.

### 3.3 Plugins, and the flag that makes a session-scoped offer possible

A plugin is a directory with `.cursor-plugin/plugin.json` and any of `hooks/`,
`mcp.json`, `rules/`, `skills/`, `commands/`, `agents/` **[measured]**. Hook
commands inside one get `CURSOR_PLUGIN_ROOT` in their environment.

The CLI takes `--plugin-dir <path>`, repeatable, and lists it in `--help`
**[measured]**. That is the Cursor equivalent of Claude Code's `--mcp-config`:
hooks, the retrieval server and the guidance rule can all be handed to one
session on its command line, with nothing written to a file the user owns.

### 3.4 What is the same on both surfaces

The desktop app ships the agent as extensions (`cursor-agent-exec`,
`cursor-agent-host`) and both contain the same hook fields as the CLI
**[measured]**: `updated_mcp_tool_output` in three files, `updated_input` in
fourteen.

**The CLI is the less reliable of the two.** The documentation gives no list of
which events fire in the CLI, and the forum record is uneven **[forum]**: in
January 2026 only the shell hooks fired; by April `postToolUse` and
`sessionStart` did; in November, on 2026.11.05, a user saw `sessionStart` and
`preToolUse` fire from a project file while `~/.cursor/hooks.json` was not
loaded at all on macOS — staff said it should be, and the thread is open.
Another open report has no hooks firing on Linux at 2026.08.11.

On this machine none of that reproduced **[spike]**: on Linux, build
2026.09.26, under `--print`, `sessionStart`, `preToolUse`, `postToolUse`,
`beforeShellExecution`, `afterShellExecution` and `afterMCPExecution` all
fired, and they fired from all three sources tried — the project file,
`~/.cursor/hooks.json`, and a `--plugin-dir` plugin. `stop` did not fire under
`--print`, as staff say. In the desktop Agent panel (3.28.7) the same events
fired from the project file, and `stop` fired as well **[spike]**. macOS and
the current CLI build remain **[unverified]**.

### 3.5 What Cursor already does with large output

Measured before anything was rewritten **[spike]**, and it changes what there
is to win:

| Tool | Result size | What the model receives | What `postToolUse` receives |
|---|---|---|---|
| Shell | 109 KB (`seq 1 20000`) | no output inline — a notice that it was written to a file under `~/.cursor/projects/…/agent-tools/`, with its size and line count | the **full** output, 129 KB |
| MCP | 32 KB | the text, inline | the text |
| MCP | 164 KB, 1.1 MB | a file reference, no text | `{"content":[{"type":"text","text":""}],"isError":false}` — **empty, and no path** |

The shell call carries `fileOutputThresholdBytes: "40000"`; the MCP threshold
lies between 32 KB and 164 KB and is presumably the same. So a single tool
result never puts more than about 40 KB into the conversation, with or without
this project. Two consequences:

- The saving is on results **under** the threshold, plus the follow-up `Read`
  calls a model makes to look inside a spilled file — a 109 KB shell output
  compressed before Cursor captures it came back as 120 bytes inline, where
  uncompressed it was a file the model would have to open.
- A `postToolUse` hook **cannot compress a spilled MCP result**: it is handed an
  empty string. §6.2 deals with that.

A shell call also carries `timeout: 30000` and
`timeoutBehavior: TIMEOUT_BEHAVIOR_BACKGROUND`: a command still running after
30 seconds is moved to the background, the tool call returns with no output,
and the model waits on it and then **reads its terminal file with the `Read`
tool**. `postToolUse` for a `Read` carries the path and a `content_length`, not
the content.

## 4. Goals and non-goals

**Goals**

1. Compress MCP tool results and shell output for the Cursor agent, console and
   desktop, with the engine as it is — same detection, same compressors, same
   invariants, same markers.
2. Keep every elision retrievable for the whole session, not only mid-request.
3. Leave no residue from a console session, and exactly two owned, removable
   entries from a desktop install.
4. Fail open at every step: a missing binary, a locked database, a hook timeout
   or a sandbox refusal must leave the agent with the uncompressed output it
   would have had anyway.
5. Say what was saved in the same `stats.db`, and be explicit that it is an
   estimate (§9).

**Non-goals**

- **Decoding Cursor's protobuf wire.** §11.1.
- **Compressing built-in `Read` / `Grep` / `List` / `Fetch` output.** No hook
  can, and redirecting a `Read` to a compressed copy would hand the model line
  numbers that do not match the file it is about to edit.
- **Keep-warm, cache TTL, frozen prefix.** They act on a provider request this
  process never sees.
- **Tab completion, Cmd-K, Bugbot, cloud agents.** Cloud agents run on Cursor's
  machines; a local binary is not there to be called.
- **Windows shell rewriting in the first version.** MCP results work anywhere
  the binary runs; the shell rewrite in §6.3 is POSIX `sh` and is switched off
  on Windows until a PowerShell form is written and measured.

## 5. The design in one picture

```
 Cursor agent (CLI or desktop)                       Kompressor (no daemon required)
 ─────────────────────────────                       ───────────────────────────────
 sessionStart ───────────────────────────────────▶  hook cursor ─▶ additional_context:
                                                                   the marker grammar,
                                                                   how to retrieve

 model calls an MCP tool
   └─ tool runs, result R
        └─ postToolUse {tool_output: R} ─────────▶  hook cursor
                                                      ├─ engine: compress_raw(R, role=tool)
                                                      ├─ refs.db: store fragments
                                                      └─ stats.db: one optimization event
        ◀──────────── {updated_mcp_tool_output: R'} ─┘
   model receives R'

 model calls Shell("cargo build")
   └─ preToolUse {tool_input.command} ───────────▶  hook cursor
        ◀──── {updated_input: {command: wrapped}} ──┘  (or {} — left alone)
   └─ Cursor runs the wrapped command:
        { cargo build; } > F 2>&1 ; kompressor hook emit --status $? F
                                          └────────▶  engine + refs.db + stats.db
   model receives the compressed output

 model meets  [... 412 lines omitted [ref:k_AbC…]]
   ├─ MCP: optscale_kompressor_retrieve(ref) ────▶  kompressor mcp serve (stdio)
   └─ or Shell: kompressor retrieve k_AbC… ──────▶  kompressor retrieve
```

Three things in that picture already exist and are used unchanged:
`AppCore::compress_raw`, the stdio MCP server with its retrieval guidance, and
the `retrieve` subcommand. What is new is the translation between Cursor's hook
JSON and those calls — which is the definition of a surface in this codebase
(DESIGN.md §3.2): translation, not logic.

## 6. Components

### 6.1 `kompressor hook cursor` — the hook handler

A new subcommand in `kompressor-cli`, backed by a small module
`crates/kompressor-cli/src/hook/` (`mod.rs`, `cursor.rs`, `shell.rs`). It is
not a new crate: it shares `AppCore`, the config loader and the stats sink with
`compress`, and a crate boundary would buy nothing. The agent-specific part is
kept in `cursor.rs` so a second hook-driven agent adds a file, not a rewrite.

```
kompressor hook cursor            read one event on stdin, write one reply on stdout
kompressor hook emit FILE         print FILE compressed (the tail of a rewritten shell command)
kompressor hook install cursor    §7.2
kompressor hook uninstall cursor  §7.2
kompressor hook status cursor     what is installed, who owns it, is the store reachable
```

**Contract of `hook cursor`:**

- Reads standard input to EOF, parses JSON, dispatches on `hook_event_name`.
- Writes exactly one JSON object to standard output, and nothing else there.
  Diagnostics go to standard error and to the log, as `mcp serve` already does
  for the same reason: stdout is the protocol.
- **Always exits 0 and always prints valid JSON.** Any failure — unparseable
  input, unknown event, config error, store error, deadline, a panic — produces
  `{}`. This is not tidiness: on `preToolUse`, exit 0 with invalid output
  *blocks the tool call*, and exit 2 blocks it on any hook (§3.1). The reply is
  therefore built in memory and written in one call, a panic hook writes `{}`,
  and the process never exits with a status it did not choose.
- **Never returns `permission`.** Other tools in this space answer a rewrite
  with `"permission": "allow"`, which approves the command on the user's
  behalf. A compressor has no business deciding that; the reply carries
  `updated_input` and nothing else, and approval stays where it was.
- Honours a deadline, `hooks.cursor.deadline` (default 2 s), enforced
  internally and set below the `timeout` written into `hooks.json` (5 s), so
  the handler gives up cleanly before Cursor kills it.

**Dispatch:**

| Event | Condition | Reply |
|---|---|---|
| `sessionStart` | always | `{"additional_context": <guidance>}` — the text `kompressor-mcp/src/guidance.rs` already produces, plus one line naming the shell fallback |
| `postToolUse` | `tool_name` starts `MCP:`, result ≥ `min_compress_chars`, not one of our own tools | `{"updated_mcp_tool_output": …}` if the engine shrank it, else `{}` |
| `postToolUse` | any other tool | `{}`, and a shadow stats row (§9.3) |
| `preToolUse` | Shell tool, rewrite enabled, command eligible (§6.3) | `{"updated_input": {"command": …}}` |
| anything else | | `{}` |

**Reading an MCP result.** `tool_output` is a JSON-encoded string (§3.1). The
handler decodes it and looks for text in the shapes an MCP result can take: a
`content` array has each `type: "text"` item compressed independently and every
other item passed through untouched, so an image or resource block survives
byte for byte; a bare string is one text block. A decoded value in any shape it
does not recognise is answered with `{}`. The envelope is the MCP result
itself — `{"content":[{"type":"text","text":"…"}],"isError":false}`
**[spike]**. A text item that arrives empty is a spilled result (§3.5) and is
left alone: there is nothing to compress and nothing true to replace it with.

**Our own tools are skipped**, by name: `MCP:optscale_kompressor_…`. A result
from the retrieval tool is a fragment the model just asked for; compressing it
would hand back a marker for the thing the marker pointed at. Because the
payload names the tool and not the server, every exclusion in hook mode is by
tool name.

**Other hooks on the same event.** With "last response wins" (§3.1), a second
tool that also rewrites shell commands or MCP output — RTK and lean-ctx both
install a `preToolUse` hook on `Shell` — will either overwrite this handler's
reply or be overwritten by it, depending on order. `hook install` refuses to
add a `preToolUse` entry beside another one matching `Shell` unless told to,
and `hook status` lists what else is registered on the three events.

**Process cost.** A hook is a process per event. `kompressor compress` on a
4,000-line log takes 40 ms wall and 15 MB resident, cold start included
**[measured]**. That is the budget hook mode spends on each tool call, against
tool calls that take seconds.

### 6.2 MCP results: `updated_mcp_tool_output`

The straightforward half, and it works **[spike]**: a `postToolUse` hook
returning `updated_mcp_tool_output` — as a string or as a `content` array —
replaced the result the model received, in the CLI, from a project hook and
from a plugin hook. The reply mirrors the shape that came in:

```json
{"updated_mcp_tool_output": {"content": [{"type": "text", "text": "…compressed…"}]}}
```

Its limit is §3.5: a result over the spill threshold reaches the hook empty.
That makes the comparison with fronting the servers through
`kompressor mcp serve` ([MCP-PROXY.md](../MCP-PROXY.md)), which also works with
Cursor today, less one-sided than it first looked:

| | Fronting (`[[mcp.servers]]`) | Hook (`postToolUse`) |
|---|---|---|
| Results under ~40 KB | compressed | compressed |
| Results over ~40 KB | compressed **before** Cursor sees them, so they stay inline | not reachable: spilled to a file, hook sees `""` |
| User's `mcp.json` | rewritten: every fronted server moves into our config | untouched |
| Tool names | prefixed `server__tool` | unchanged |
| Remote (`url`) servers | not supported (stdio only) | supported |
| A server's credentials | copied into a second file | never leave Cursor |

On the desktop the two halves of this were each seen, in different sessions:
the hook received a successful result in full, and a replacement was applied —
to a failed call (§12.3). The two together on one successful call
is Q2 (§12.5).

The hook is the default because it changes nothing the user owns. Fronting is
the answer for a server known to return large results, and the two can be
mixed per server: front the heavy one, let the hook handle the rest. Shadow
mode (§9.3) counts empty-text MCP results per tool, which is the list of
servers worth fronting.

### 6.3 Shell output: rewriting the command

Cursor gives no way to replace shell output after the command has run, so the
only place left is before: `preToolUse` rewrites `command` so that what the
shell prints is already compressed. This is the largest part of the saving —
build output, logs and listings are 47% of eligible tool output on the Codex
corpus and compress at 75–82% (README, *Replayed on the same traffic*) — and it
is the part with the most ways to go wrong, so it is specified narrowly.

That the mechanism works is not in question: rewriting `command` from a
`preToolUse` hook is how RTK and lean-ctx integrate with Cursor today (§11.7),
and it worked here **[spike]**. They replace a command with a quieter one; this
design keeps the command and compresses what it prints.

**The rewritten form.** For an eligible command `C`, with `F` a path the hook
chooses under `$TMPDIR`, unique per `tool_use_id`:

```sh
{
C
} > 'F' 2>&1; '/abs/path/kompressor' hook emit --status "$?" 'F'
```

`hook emit --status N FILE` prints `FILE` compressed, deletes it, and exits
`N`. If anything goes wrong it prints `FILE` unchanged and still exits `N`.

Each piece is there for a reason, and each was tried **[spike]**:

| Piece | Why | Measured |
|---|---|---|
| `{ C \n }` — a brace group, not `sh -c '…'` | `C` runs **in the shell Cursor keeps between calls**, and is embedded verbatim: no quoting pass that could change its meaning. The newline before `}` lets `C` end in a heredoc or a comment. | `cd /var/tmp` inside the group; the next call's `pwd` printed `/var/tmp`. An `export` carried over the same way. |
| `> 'F' 2>&1` — a file, not a pipe | A pipe puts `C` in a subshell and replaces its exit status. | Through `C 2>&1 \| kompressor compress`, `ls /nonexistent` returned exit 0. |
| `--status "$?"` | The model sees the exit status `C` produced, without a subshell to carry it. | `ls /nonexistent` in the brace form: `$?` was 2 at that point. |
| no shell variables, no `rm`, no `\|\| cat`, no `(exit …)` | **Cursor's allow-list rejects them** — see below. The file name is fixed by the hook, and clean-up and fail-open move into `emit`. | |
| absolute path to the binary | The shell Cursor spawns may not have the installing user's `PATH`. | |

**Approval is evaluated on the rewritten command** **[spike]**. This is the
finding that reshaped the form. With `Shell(echo)` on the CLI's allow-list and
no `--force`:

| What the hook returned for `echo alpha-one` | Result |
|---|---|
| nothing (control) | ran |
| the first draft of this design — `__kf=…; { C } >"$__kf" 2>&1; __ks=$?; kompressor … \|\| cat "$__kf"; rm -f "$__kf"; (exit $__ks)` — with `kompressor`, `cat` and `rm` also allow-listed | **rejected** |
| `C \| kompressor compress`, `{ C; } 2>&1 \| kompressor compress` | ran |
| `C \| kompressor compress; (exit ${PIPESTATUS[0]})` | rejected |
| `set -o pipefail; C \| kompressor compress` | rejected |
| `{ C } > /tmp/f 2>&1; echo "status=$?"; kompressor compress /tmp/f` | ran |

So brace groups, redirections, `;`, `$?` and pipes pass Cursor's command
parser; subshells and `set` do not, and the first draft failed on something
among its variable assignments, `||` and `(exit …)`. A user on an allow-list
needs one new entry, `Shell(kompressor)`, which `hook install` offers to add to
`~/.cursor/cli-config.json` and never adds unasked.

It also cuts the other way, and the form is designed around that too: a hook
that rewrote `seq 1 3` — **not** allow-listed — to `kompressor version` saw it
run. A rewriting hook can turn a command the user would have been asked about
into one they are not. This design must never do that, which rules out any
form that hides `C` from the parser — `kompressor run -- C` and
`sh -c 'C'` in particular. In the brace form `C`'s own executables remain
visible to the allow-list, and an executable *inside* the group that is not
allow-listed is still rejected **[spike]** (§12.6, Q1). The allow-list also
judges this binary's own word literally, so the rewrite names it as an entry
can match — bare, or by its unquoted absolute path — and is offered only when
the CLI's settings would approve it.

**What the model sees** **[spike]**. The tool call in the transcript keeps the
command the model wrote, in both its arguments and its result; asked what it
ran, the model named the original. The rewritten text appears in one place: the
header of a backgrounded command's terminal file, which the model reads.
`beforeShellExecution`, `afterShellExecution` and `postToolUse` all receive the
rewritten command, so `postToolUse` can tell a wrapped call from a bare one.

**What is not rewritten.** `preToolUse` answers `{}` and the command runs as
the model wrote it when any of these holds:

| Case | Why it is left alone |
|---|---|
| the command already invokes this binary | `kompressor retrieve …` must return the fragment whole |
| it backgrounds a job: an unquoted `&` at the end of a list, `nohup`, `disown`, `setsid` | the job would keep writing to a file that has been removed |
| it is interactive or needs a terminal: `ssh` without a command, `vim`, `less`, `top`, `watch`, `tmux`, a REPL with no script, `sudo` without `-n`, `git rebase -i`, `git add -p` | stdout is no longer a TTY once redirected |
| it already redirects standard output of the whole command to a file | there is nothing to compress |
| it is on `hooks.cursor.shell.skip`, by default the commands whose output is small by nature: `cd`, `pwd`, `echo`, `export`, `mkdir`, `touch`, `rm`, `mv`, `cp`, `chmod`, `which`, `git add`, `git commit`, `git checkout`, `git switch`, `git push` | a rewrite that cannot save anything is pure risk |
| the command does not parse as balanced — unterminated quote, unbalanced `{`/`(` | embedding it in a brace group would change what it means |
| the platform is Windows | §4 |

The classifier is a tokenizer, not a shell parser: it recognises quotes,
escapes, heredoc starts and the control operators, which is enough to answer
the questions in the table. Where it cannot decide, it does not rewrite. The
bias is deliberate — a missed rewrite costs tokens, a wrong one costs a
confused agent.

**Long commands** **[spike]**. Redirected, `C` no longer has a terminal on
stdout, so tools drop colour and progress redraws; for the model that is an
improvement. Nothing is shown until `C` finishes, and that turned out to cost
little: Cursor backgrounds any command after 30 seconds whether or not it has
printed (§3.5). A rewritten `sleep 40; seq 1 5` was backgrounded, awaited, and
its output read from the terminal file, exactly as the bare command was. What
is lost is the partial output a model could have read from that file *while* a
long build ran. `hooks.cursor.shell.enabled = false` turns the rewrite off
without touching the MCP half.

**Residual exposure.** With `|| cat` gone, a binary that disappears between
the rewrite and the run leaves the command's output in `F` and the model with
`command not found`. `F` is not deleted in that case, and the error names it.
An install that moves is caught at the next `preToolUse`, which runs the same
binary and so cannot rewrite to a path that does not exist.

### 6.4 Retrieval

A marker the model cannot resolve is a lie about recoverability
(design/MCP-PROXY.md, *The retrieve tool must arrive…*), so hooks are never
installed without a way to retrieve. Two ways, offered together:

1. **The tool server.** `kompressor mcp serve` registered as
   `optscale_kompressor`, exactly the registration `wrap claude` writes
   (`wrap/mcp.rs`). Its `instructions` carry the marker grammar.
2. **The shell.** `kompressor retrieve k_…` already exists and needs no
   registration. The `sessionStart` context names it, so retrieval works in a
   session where MCP servers are disabled, unapproved, or over Cursor's tool
   limit.

**The store is pinned**, for the reason DESIGN.md §14.1a gives: the hook, the
`emit` step and the tool server are three processes started from three working
directories. `refs.path` is resolved to an absolute path when the plugin or the
install is written, and all three are handed the same `KOMPRESSOR_CONFIG`.

### 6.5 Configuration

A new table, off by default, following the shape of `[proxy.intercept]`:
`#[serde(default, deny_unknown_fields)]`, a hand-written `Default`, and the
`KOMPRESSOR__HOOKS__CURSOR__*` overlay for free.

```toml
[hooks.cursor]
# Master switch. Installed hooks answer {} when this is false, so compression
# can be turned off without editing a file Cursor owns.
enabled = true

# Give up and answer {} after this long. Must stay below the `timeout` the
# installer writes into hooks.json (5 s).
deadline = "2s"

# Record what every tool returned and what the engine would have removed,
# without changing anything. See §13.3.
shadow = false

[hooks.cursor.mcp]
enabled = true
# Tools whose results are forwarded untouched. Matched against the name after
# `MCP:`; the hook is not told which server a tool belongs to.
skip_tools = []

[hooks.cursor.shell]
enabled = true
# Commands never rewritten, matched on the first word (two for `git`).
skip = ["cd", "pwd", "echo", "export", "mkdir", "touch", "rm", "mv", "cp",
        "chmod", "which", "git add", "git commit", "git checkout",
        "git switch", "git push"]
# Captured output above this is printed uncompressed rather than read into
# memory. The engine is not streaming.
max_capture_bytes = 67108864
```

Engine options come from `[engine]` as on every other surface. A per-surface
`[hooks.cursor.engine]` override, in the manner of
`[proxy.providers.anthropic.engine]`, is deferred until a measurement asks for
one.

### 6.6 Statistics

Hook processes write `stats.db` directly, as the stdio MCP server does; WAL
makes that safe beside a running `serve`.

| Column | Value in hook mode |
|---|---|
| `surface` | `cli` in the first version. The column is CHECK-constrained to `api, mcp, proxy, cli` and SQLite cannot alter a CHECK without rebuilding the table, so a dedicated `hook` value is a later migration, not a prerequisite. |
| `provider` | `cursor` — the discriminator until `surface` has its own value |
| `model` | the hook's `model` field |
| `request_id` | `generation_id`, so the events of one agent turn join |
| `tenant` | the local default; a hook has no credential to resolve |

**No `proxy_requests` rows are written.** That table is the provider's
accounting, and hook mode has none. Every report built on it — the request
economy, cache reads and writes, the net-of-retrieval bill — is empty for
Cursor, and §9 says what stands in for it.

## 7. Wiring: console and desktop

### 7.1 Console: `kompressor wrap cursor-agent`

The `Wiring::Unsupported` entry becomes a fourth automatic wiring:

```rust
pub enum Wiring {
    Launch,
    Manual { where_: &'static str },
    Unsupported { why: &'static str },
    /// The tool is not pointed at the proxy at all: it is handed hooks that
    /// call this binary, and no proxy is started.
    Hooks { flag: &'static str },          // "--plugin-dir"
}
```

`wrap cursor-agent` then:

1. Resolves the configuration and pins `refs.path` and `stats.path` to absolute
   paths, writing the session configuration under `$KOMPRESSOR_HOME/run/`, as
   `wrap claude` does for its tool server.
2. Writes a plugin directory beside it:

   ```
   $KOMPRESSOR_HOME/run/cursor-plugin-<session>/
       .cursor-plugin/plugin.json     name, version, "mcpServers": "./mcp.json"
       hooks/hooks.json               sessionStart, preToolUse, postToolUse
       mcp.json                       optscale_kompressor → kompressor mcp serve
   ```

3. Runs `cursor-agent --plugin-dir <that directory> <the user's arguments>` in
   the foreground. **No proxy is started and no port is resolved** — the steps
   of the wrap diagram in DESIGN.md §14.1 above "launch" are skipped for this
   wiring.
4. On exit, removes the directory and prints the session line from `stats.db`,
   filtered to this session's `conversation_id`s.

Tried with a hand-made plugin **[spike]**: its hooks fired, its `sessionStart`
context reached the model, and its MCP server was offered under the name
`plugin-<displayName>-<server>` — so the plugin's `displayName` is part of
what the model sees and is kept short. With a user-level `hooks.json` present
as well, **both** sets of hooks fired on every event, which is why `wrap`
checks for an install first (§7.2).

A session killed without cleanup leaves an inert directory under `run/` and
nothing else, which is the property `--mcp-config` buys for Claude Code. There
is no `unwrap cursor-agent`.

The binary is found as `cursor-agent`, then `agent`; the installer puts both on
`PATH` and the build names itself `agent` **[measured]**.

### 7.2 Desktop: `kompressor hook install cursor`

The editor is not launched by `wrap`, so there is no command line to put a flag
on. The registration has to be durable, and this design accepts that — it is
the row the command-line offer loses on in DESIGN.md §14.1a, taken here because
there is no alternative, and made as small and as owned as it can be.

`install` writes two things, by default at user level, with `--project` for
`<workspace>/.cursor/`:

**`~/.cursor/hooks.json`** — three entries added to whatever is there:

```json
{
  "version": 1,
  "hooks": {
    "sessionStart": [{ "command": "'/usr/bin/kompressor' hook cursor", "timeout": 5 }],
    "preToolUse":   [{ "command": "'/usr/bin/kompressor' hook cursor", "timeout": 5, "matcher": "^Shell$" }],
    "postToolUse":  [{ "command": "'/usr/bin/kompressor' hook cursor", "timeout": 5, "matcher": "^MCP:" }]
  }
}
```

The matchers keep a process from being spawned for tool calls the handler would
answer `{}` to anyway. With `shadow = true` the `postToolUse` matcher is written
as `""`, which matches every tool. Cursor watches the file and reloads it, so an
install takes effect without a restart **[docs]**.

`sessionStart` may also return `env`, which Cursor passes to later hook runs in
the session **[docs] [spike]** — a variable returned there was present in every
later hook's environment, and `additional_context` returned beside it was
quoted back by the model. The handler uses it to hand `KOMPRESSOR_CONFIG` to the
`preToolUse` and `postToolUse` runs, so the path is resolved once.

**`~/.cursor/mcp.json`** — one server, `optscale_kompressor`.

Both edits go through the existing claim-and-restore machinery in
`wrap/settings.rs`, with one difference from `--attach`: the claim is not tied
to a live process. Ownership is recorded in a sidecar
(`$KOMPRESSOR_HOME/installed/cursor.json`) listing exactly the entries written;
`uninstall` removes those entries and nothing else, and leaves a file it emptied
as `{"version":1,"hooks":{}}` only if it created it. An entry the user has
edited since is reported and left in place rather than removed.

Because the CLI reads the same two files **[measured] [docs]**, an install is
meant to cover every `cursor-agent` session not started through `wrap` as well
— subject to the open report that the CLI does not load the user-level file on
macOS (§3.4). A session that *is*
started through `wrap` on an installed machine would get the hooks twice; `wrap`
detects the install and skips its plugin.

**Why not `wrap cursor --attach`.** `--attach` means "hold a proxy up until
Ctrl-C and restore on exit". There is no proxy here and nothing that needs a
process to stay alive, so borrowing the verb would promise a lifetime that does
not exist.

**Why not a marketplace plugin.** It is the right long-term distribution — one
click, updated by Cursor — but a published plugin cannot contain the absolute
path of a locally installed binary, and it would have to find `kompressor` on a
`PATH` the editor may not share. It is listed under Tier 7 (§13.9), after the mechanism
is proven.

**Managed machines.** A team can distribute the same three entries through the
enterprise path (`/etc/cursor/hooks.json`) with its own tooling. `install
--print` emits the JSON without writing anything, for that use.

### 7.3 What happens to `wrap cursor`

See §11.2. The entry is kept, re-labelled, and its instructions point at
`hook install cursor` first.

## 8. Invariants and failure policy

The four invariants (CONTRIBUTING.md) hold in hook mode because the engine
enforces them and the hook adds no logic of its own:

| Invariant | In hook mode |
|---|---|
| Never larger | The handler returns `{}` unless the engine reports a strict shrink. `hook emit` prints the original when it does not. |
| Every shrink accounted for | One optimization event per compressed result, as on every surface. |
| Deterministic | Same engine, same options. It matters less here — a result is compressed once and never re-sent — but it is what makes a replayed transcript reproduce. |
| Fail open | Four independent layers: the handler answers `{}` on any error; it enforces its own deadline; Cursor ignores a failed or timed-out hook; and `hook emit` prints the captured output unchanged if it cannot compress it. |

**What fail-open cannot cover.** Two cases, both in the shell half. The binary
is removed between the rewrite and the run: the allow-list findings in §6.3
took `|| cat` out of the command, so the output stays in the capture file and
the model is told the command was not found. And a `$TMPDIR` that is not
writable, where the redirect itself fails and `C` does not run; the hook
creates the capture file itself before returning the rewrite, and declines to
rewrite if it cannot.

**Sandboxed shells.** The CLI can run commands under a sandbox (`--sandbox`,
policies in `~/.cursor/sandbox-policies/`). `hook emit` runs *inside* it, and
`refs.db` lives outside the workspace. Under `--sandbox enabled --force` on
this machine **[spike]** the hooks reported `sandbox: true`, the hook process
wrote under `$HOME` freely, and so did the sandboxed shell: the rewritten
command created `refs.db` in `~/.cache/…` and returned compressed output with a
working handle. That is one policy on one machine, and it is more permissive
than the name suggests — a managed policy may not be. So the fallback stays:

1. Try the store. If it opens, proceed normally.
2. If not, compress in `lossless` mode: folds that need no handle still apply,
   nothing is elided behind a reference that could not be stored.
3. Record nothing (the stats file is equally unreachable), and say so once on
   standard error.

Whether a sandboxed shell can reach a loopback listener was not settled: the
test request was refused inside and outside the sandbox alike, because nothing
was listening. Hook mode does not depend on it.

## 9. What this saves, and what it cannot measure

### 9.1 Why compress-at-entry is enough

In the proxy, the hard problem is that the client re-sends its history every
turn and the compressed bytes must be identical each time, or the provider's
prefix cache breaks. In Cursor the client sends each tool result once. Whatever
bytes go up are the bytes the conversation holds from then on. A result
compressed at entry is as stable as one that was not, by construction, and
every later turn in that conversation carries the smaller version.

Cursor prices input, cache write, cache read and output separately, with cache
reads well below input — for one listed model $5.00 / $6.25 / $0.50 / $25.00
per million **[docs]**. A tool result is therefore paid for once at the write
price and then at the read price on each later turn, which is the same
arithmetic as the proxy's and the reason the saving is largest on long
sessions with large early outputs. On plans with an included pool the unit is
pool consumption rather than dollars; the pricing page does not say how tool
output is counted and that tool results are billed as input is an inference.

### 9.2 What is out of reach

- **The cached prefix.** Keep-warm, adaptive TTL and tail-only compression are
  the larger share of the Claude Code result (README, *Where the money went*)
  and none of them has anything to act on here.
- **File reads and searches.** In Codex these go through the shell and are
  compressed; in Cursor they are built-in tools and are not. The Codex corpus's
  category shares therefore overstate what hook mode reaches. Shadow mode can
  size them but not sample them: `postToolUse` for a `Read` carries a
  `content_length` and no content **[spike]**. How much of a
  Cursor session's tool output is shell and MCP is **not known** and is the
  first thing shadow mode measures (§13.5).
- **Cross-turn de-duplication** (`engine::crossturn`) needs the conversation,
  which a hook does not have. A per-conversation ledger keyed by
  `conversation_id` could restore part of it and is left as future work.

### 9.3 What gets reported

Kompressor's rule is that savings are the difference between real bills
(README, *Results*). Hook mode cannot meet that rule from its own data, and the
reports must not pretend otherwise:

- **Reported from `stats.db`:** tokens and characters removed per result, by
  content type, by tool. This is *gross removal at entry*, not money.
- **Not reported:** a dollar figure. A removed token is paid for once per later
  turn at a price Cursor sets, and neither the turn count nor the price is
  visible to a hook.
- **The honest number comes from Cursor's own usage export** — an A/B over the
  same tasks with hooks on and off, billed tokens compared. That is the
  acceptance test of §13.8, in the same shape as the Terminal-Bench
  runs in BENCHMARK.md, driven through `cursor-agent --print`.

**The desktop reports usage to hooks** **[spike]**. Its `stop` event carries
`input_tokens`, `output_tokens`, `cache_read_tokens` and `cache_write_tokens`
for the turn. That is not a bill — there is no price, and the CLI under
`--print` does not fire `stop` — but it is the provider-side count this section
said was out of reach, per turn and per conversation. Recorded alongside the
removal figures it makes an on/off comparison possible from Kompressor's own
database on the desktop, and recording it is step 7.2 (§13.9).

**Shadow rows.** With `hooks.cursor.shadow = true`, `postToolUse` runs the
engine on every tool's output and records what it would have removed, without
returning it. Shadow rows carry `mode = 'off'` and the would-be `tokens_out`,
so they are separable from real compression and from each other by
`content_types`.

## 10. Security and privacy

- **Hooks run as the user, with the user's environment.** The handler reads
  only standard input and its configuration. It executes nothing from the
  event: the shell rewrite is string construction around `C`, and `C` is run by
  Cursor's shell, under Cursor's permission model, not by this binary.
- **The rewrite embeds `C` verbatim inside a brace group.** It adds no quoting
  layer, so it introduces no new injection surface: anything `C` could do
  rewritten, it could do before. The classifier's refusal to rewrite unbalanced
  input is what keeps `C` from closing the group early.
- **Fragments stay local.** `refs.db` holds the elided text on the user's disk,
  under the same TTL as every other surface. Nothing is sent anywhere by hook
  mode; there is no network call in the handler.
- **Tool output does reach a second place on disk** that it would not have
  otherwise. A team in Cursor's privacy mode should know that `refs.db` is now
  part of where session content lives, and `refs.ttl` governs for how long.
- **No credentials are written.** The installed entries name a binary and a
  subcommand. The MCP registration's `env` carries `KOMPRESSOR_CONFIG` only.
- **No traffic is intercepted and no Cursor binary or protocol is modified.**
  Hooks, `mcp.json` and plugins are extension points Cursor publishes for this
  purpose.
- **Managed accounts.** A team policy can disable user and project hooks. Then
  `install` succeeds and nothing fires; `hook status cursor` reports the last
  time a hook was actually called so this is visible rather than silent.

## 11. Alternatives considered

### 11.1 Intercept `api2.cursor.sh` and decode the protobuf

`[proxy.intercept]` can terminate TLS for any allow-listed host, so the bytes
are obtainable. Rejected, for reasons that compound:

- The schema is private and unversioned. A wire walker for it would be
  maintained by reverse engineering against builds that ship every few days,
  and its failure mode is silent corruption of an agent's tool results.
- The stream is bidirectional HTTP/2 with tool results multiplexed among
  control frames; rewriting one means re-framing a Connect stream in flight.
- It would deliver exactly what hooks deliver — tool output before the model
  sees it — and nothing more, because the prompt is assembled server-side.
- Cursor's terms prohibit reverse engineering and modifying the service, and its
  acceptable-use policy prohibits circumventing protective measures and
  tampering with usage metering **[docs]**. Staff have said that a local proxy
  calling the private endpoints "doesn't change the analysis" **[forum]**. They
  have not ruled on a proxy that rewrites the official client's own traffic,
  and this project should not be the test case. Hooks, `mcp.json` and plugins
  are documented extension points and raise none of this.

One piece of prior art sits on this path without decoding anything: LiteLLM
relays the CLI's subscription traffic as opaque bytes, through an undocumented
`--agent-endpoint` flag **[measured: the flag is in the build]** and
`network.useHttp1ForAgent`. That gives routing and nothing a compressor can
use.

### 11.2 Bring-your-own-key with a base-URL override (`wrap cursor` today)

This is a dead end for a local proxy, and the entry should say so.

- "All requests are routed through Cursor's servers for final prompt building",
  the user's key included **[docs]**. The request to an overridden base URL is
  made from there. Staff confirm that loopback and LAN addresses do not work
  and that a public HTTPS tunnel is required **[forum]**.
- In Agent mode the override is sent OpenAI Responses requests, not Chat
  Completions **[forum]** — a shape the proxy does serve, but only if it can be
  reached.
- The CLI has no bring-your-own-key at all, as of 2026-11-01 **[forum]**.

So `kompressor wrap cursor` prints a `http://127.0.0.1:8817/v1` that Cursor's
backend cannot call, which is why no request has ever arrived. Exposing the
proxy through a public tunnel would make it work and would put an
unauthenticated compression proxy, and the user's key, on the internet; that is
not something to print instructions for. The entry is changed to explain this
and to point at `hook install cursor`. Headroom's Cursor support is the same
manual base-URL instruction and has the same limit.

### 11.3 The CLI's local-provider mode

The CLI build contains a mode in which it calls a model provider directly:
hidden options `--base-url` ("OpenAI-compatible or Anthropic Messages"),
`--local-agent-api-key`, `--authless`, and the variables
`CURSOR_LOCAL_AGENT_BASE_URL` and `CURSOR_AGENT_CLI_LOCAL_MODE`
**[measured]**. It would have been the best integration available — the two
wire formats the proxy already serves, and so every proxy feature.

It is not reachable **[spike]**. With the variable set and without it, the
shipped binary answers `Error: --base-url can only be used with
agent-cli-local` and sends nothing; a recording server on the named port
received no request. `agent-cli-local` is a separate build that Cursor does
not distribute. Nothing in this design depends on it. If Cursor ships it, the
`cursor-agent` entry gains an ordinary `Wiring::Launch` beside its hooks.

### 11.4 MCP fronting only

Works today with no new code (§6.2 table) and covers MCP results. It does not
touch shell output, which is where most of the bulk is expected to be, and it
costs the user their `mcp.json`. Kept as the documented fallback.

### 11.5 Redirecting `Read` to a compressed copy

`preToolUse` can rewrite a `Read`'s `file_path`, so a read could be pointed at
a compressed temporary copy. Rejected: the model edits by line and by exact
text, and a file whose bodies are elided is not the file it will edit.
`engine::read_lifecycle` exists precisely because reads need more care than
other output, and it needs the conversation to do its job.

### 11.6 Cursor's Claude-compatibility hooks

Cursor reads `.claude/settings.json` hooks too **[measured: the paths are in
the loader]**. Relying on that would tie Cursor support to a compatibility
layer and to Claude Code's hook contract, which has no equivalent of
`updated_mcp_tool_output` to map from. Staff also describe imported hooks in
the CLI as receiving a Cursor-shaped payload and other unintended behaviour
**[forum]**. Native `hooks.json` is used.

### 11.7 Prior art: command substitution

RTK (`rtk init --agent cursor`) and lean-ctx install a `preToolUse` hook on
`Shell` and return `updated_input` with a different command — `git status`
becomes a wrapper that prints a terser status. lean-ctx also redirects `Read`
and `Grep`. This confirms the hook this design depends on, and it is a
different trade:

| | Command substitution | This design |
|---|---|---|
| What runs | a different program, chosen per command from a table | the command the model wrote |
| Coverage | commands the table knows | any command whose output the engine can read |
| Removed detail | gone | behind a handle, retrievable |
| MCP results | not covered | covered |

The two do not compose on one event (§6.1, *Other hooks on the same event*).

## 12. Test results

Sixteen sessions against a real Cursor on 2026-11-25: thirteen
`cursor-agent --print` sessions (Linux, build 2026.09.26-dd393fe, default
model) and three in the desktop Agent panel (3.28.7, the user's normal
approval settings, model `grok-4.7`). No code from this design was involved: a
recording hook appended every event to a file and answered from a table, a
fixture MCP server returned a chosen number of lines, and where a test needed
real compression the rewritten command called `kompressor compress` from the
installed 2.6.0. Appendix A has the method and Appendix B the captured
payloads.

### 12.1 Scorecard

| Capability the design needs | CLI | Desktop | Verdict |
|---|---|---|---|
| Hooks fire from a project `hooks.json` | pass | pass | proven |
| Hooks fire from `~/.cursor/hooks.json` | pass | not run | proven on CLI |
| Hooks and an MCP server offered through `--plugin-dir` | pass | n/a | proven |
| `sessionStart` → `additional_context` reaches the model | pass | pass | proven |
| `sessionStart` → `env` reaches later hooks | pass | pass | proven |
| `postToolUse` receives an MCP result in full (under the spill threshold) | pass | pass | proven |
| `updated_mcp_tool_output` replaces what the model receives | pass | pass, on a failed call only | proven on CLI; one check left on desktop (Q2) |
| `preToolUse` → `updated_input.command` rewrites a shell command | pass | pass | proven |
| The model's transcript keeps the original command | pass | pass | proven |
| Brace-group form: `cd` / `export` persist | pass | not run | proven on CLI |
| Brace-group form: `C`'s exit status available as `$?` | pass | pass | proven |
| Brace-group form passes a CLI allow-list | pass | not run | proven on CLI |
| Compressed output returned inline with a working handle | pass (109 KB → 120 B) | pass | proven |
| Works with the shell sandbox on | pass | pass (on by default) | proven for the default policy |
| Long command (> 30 s) still returns its output | pass | not run | proven on CLI |
| `stop` reports token usage | does not fire under `--print` | pass | desktop only |
| Local-provider mode (`--base-url`) | **fail**: refused | n/a | dropped (§11.3) |
| Compress an MCP result over the spill threshold from a hook | **fail**: hook gets `""` | not run | not possible; use fronting (§6.2) |
| First-draft shell form under an allow-list | **fail**: rejected | not run | replaced (§6.3) |

### 12.2 CLI

| # | Question | Answer |
|---|---|---|
| S1 | Which hooks fire in the CLI, from which source, and with what payloads? | `sessionStart`, `preToolUse`, `postToolUse`, `before`/`afterShellExecution`, `afterMCPExecution` fire under `--print` from the project file, the user file and a `--plugin-dir` plugin. `stop` does not. Shell `tool_input` is `{command, cwd, timeout}`; shell `tool_output` is `{"output":…,"exitCode":…}` as a string; an MCP tool is `MCP:<tool>` and its `tool_output` is the MCP result as a string. Matchers `^Shell$` and `^MCP:` select as expected. Appendix B has the captures. |
| S2 | Is approval evaluated before or after the rewrite? What does the model see? | **After.** The allow-list judges the rewritten command, in both directions (§6.3). The model's transcript keeps the original. |
| S3 | Does the rewrite work under `--sandbox enabled`? | Yes on this machine, including writes to a store under `$HOME` (§8). Not a general answer. |
| S4 | Long commands, and shell state through the brace group? | Backgrounded at 30 s with or without the rewrite; output recovered from the terminal file. `cd` and `export` persist through the brace group. Exit status is available as `$?` after it. |
| S5 | Is the local-provider mode usable? | No. The shipped binary refuses `--base-url` (§11.3). |
| S6 | Does Cursor already cap large output? | Yes: anything over 40,000 bytes is written to a file and the model gets a reference (§3.5). |
| S7 | Do `sessionStart`'s `additional_context` and `env` arrive? | Both do, from a project hook and from a plugin hook. The size cap was not probed. |
| S8 | Does `updated_mcp_tool_output` work on a spilled result? | The replacement is honoured, but the hook is given an empty string to work from, so there is nothing to compress (§3.5, §6.2). |

### 12.3 Desktop

Project-level hooks; same recording hook, same fixture.

| Question | Answer |
|---|---|
| Do the hooks fire? | Yes: `sessionStart`, `preToolUse`, `postToolUse`, `before`/`afterShellExecution`, `afterMCPExecution`, and `stop`. Payload keys match the CLI's, plus `model_id` and `model_params`. |
| Does the shell rewrite work? | Yes. `echo alpha-one` rewritten to `echo beta-two` ran, and the model reported running the original. The brace form around `seq 1 20000` returned the compressed output inline with a handle; around a failing `ls`, `$?` was 2. Shell hooks reported `sandbox: true` — the desktop sandboxes by default — and the store under the scratch directory was still written. |
| Does `updated_mcp_tool_output` work? | It is honoured — but the test was not the one intended. The fixture server was not enabled in the editor, so both MCP calls **failed** (`Error: MCP server does not exist: kspike`, seen in a second session with the replacement off). The hooks still fired, were handed `{}` as the result, and the replacement they returned reached the model in place of the error. |
| What does the hook see of a successful MCP result? | The same as on the CLI. With the server enabled (third session, replacement off): `{"content":[{"type":"text","text":"small-ok"}],"isError":false}` for the small tool and the full 10.7 KB for a 200-line result, in `postToolUse.tool_output` and `afterMCPExecution.result_json` alike. The model had to address the server as `project-0-ws-kspike`; the hook's `tool_name` stayed `MCP:<tool>` and `mcp_server_name` stayed `kspike`. |
| And of a failed one? | **`{}`**, and a hook that answers it replaces an error with its own text — which is what happened in the first session. The handler must therefore never return `updated_mcp_tool_output` for a result it could not parse into text; §6.1 already requires that, and this is why. |
| Do `additional_context` and `env` arrive? | Yes, both. |
| Anything new? | The desktop `stop` event carries the turn's usage: `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens` — here 103,689 / 3,086 / 91,776 / 0. The CLI under `--print` does not fire `stop` at all. §9.3 uses this. |

What the approval prompt displayed for a rewritten command was not recorded.

### 12.4 What the results changed

Five things in this document are different from the first draft because of
these tests. They are listed because each is a place where the obvious design
was wrong.

| Finding | Was | Now |
|---|---|---|
| Cursor writes any tool result over 40 KB to a file and gives the model a reference (§3.5) | the saving was assumed to scale with output size, as on Codex | bounded: results under the threshold, plus the follow-up reads a compressed inline result avoids. Sizing it on real sessions is Tier 3 (§13.5) and gates everything after it |
| A hook is handed an empty string for a spilled MCP result (§3.5) | the hook replaced fronting | the hook is the default, fronting is kept for servers with large results (§6.2) |
| Approval is evaluated on the rewritten command, and the first-draft form is rejected by an allow-list (§6.3) | variables, `\|\| cat`, `rm`, `(exit …)` in the shell | `{ C } > F 2>&1; kompressor hook emit --status "$?" F` — nothing but a brace group, a redirect and one command; clean-up and fail-open moved into the binary |
| A rewriting hook can turn an unapproved command into an approved one (§6.3) | not considered | forms that hide `C` from Cursor's parser are ruled out; the rewrite is limited to run-everything sessions until Q1 passes |
| A failed MCP call reaches the hook as `{}`, and a reply to it replaces the error (§12.3) | reply whenever the engine shrank something | never reply to a result that did not parse into text |

And two things got better: the desktop `stop` event reports token usage, so a
usage comparison is possible from Kompressor's own data (§9.3); and a long
command costs nothing extra, because Cursor backgrounds it at 30 seconds
either way (§6.3).

### 12.5 Open tests

None needs code from this design; each can be run with the same recording
hooks. §13 assigns each to the tier whose entry gate it is.

| # | Question | Experiment | Blocks |
|---|---|---|---|
| Q1 | **Answered for the CLI (§12.6): yes, still rejected.** Open: what the desktop approval prompt shows for a rewritten command. | The brace form around a command that is not on the allow-list, with `kompressor` on it; and one rewritten command in the desktop under ask-every-time. | the shell rewrite outside run-everything sessions (Tier 5) |
| Q2 | Desktop: is the replacement applied to a *successful* MCP result, and is the spill threshold the CLI's? | The fixture's 200-line and 3,000-line calls with the replacement on. | closing Tier 4 on the desktop; low risk |
| Q3 | Do the hooks fire on macOS, on the current CLI build, and from the user-level file in the desktop app? | The same recording hooks; one shell command and one MCP call each. | Tier 0 |
| Q4 | A restrictive sandbox policy: can `hook emit` reach the store, and is `$TMPDIR` writable? | A policy denying writes outside the workspace. | §8's fallback being tested rather than assumed (Tier 5) |
| Q5 | What share of a real session's tool output is Shell, MCP, Read, Grep — and how much of it is under 40 KB? | Shadow mode over a week of real use. | **whether Tiers 4 and 5 are built at all** (Tier 3) |
| Q6 | The size cap on `additional_context`. | Growing strings until one is dropped. | the length of the guidance (Tier 4) |
| Q7 | Which construct sank the first-draft form — the assignment, `\|\|`, or `(exit)`? | One session, one construct per command. | nothing; only if `\|\| cat` is wanted back |

### 12.6 Q1, answered (2026-11-25, CLI 2026.11.22-e9042b2)

Sessions under `-p` with **no `--force`**, the allow-list set per run in the
workspace's `.cursor/cli.json` (which this build merges with the global file),
shell compression on through `wrap cursor-agent`:

| Allow-list | Command | Bare | Rewritten |
|---|---|---|---|
| `Shell(seq)` | `seq 1 3` | ran | — |
| `Shell(seq)` | `uname -a` | refused | — |
| `seq`, `kompressor`; binary on `PATH` | `seq 1 20000` | — | **ran**, 109 KB → 84 chars with a handle |
| `seq`, `Shell(/abs/kompressor)`; not on `PATH` | `seq 1 20000` | — | **ran**, compressed |
| `seq` and both `kompressor` entries | `uname -a` | — | **refused** |
| same | `seq 1 3 && uname -a` | — | **refused** |
| `seq` only | `seq 1 20000` | — | refused |

The first rewrite form, with the binary as a quoted absolute path, was refused
even with `Shell(seq)` and `Shell(kompressor)` allow-listed. Varying one piece
at a time with a fixed-reply hook isolated it: the allow-list matches the
command word **literally** — `Shell(kompressor)` only a bare `kompressor`,
`Shell(/abs/path)` only that path unquoted, neither a quoted path. The quoted
capture file, `"$?"`, `--config` and the other flags all passed. Hence
`shell::program_word`, `hooks.cursor.shell.only_when_approved` (rewrite only
when the CLI's settings would run the result), and `--allow-list` adding both
entries.

A full e2e round on the same build then added two findings. A workspace's
`.cursor/cli.json` **replaces** the global `permissions.allow` rather than
adding to it — the CLI's config merge recurses into objects and replaces
arrays — so with `Shell(kompressor)` global and `Shell(seq)` alone in the
project file, the rewritten `seq` was refused; the hook now merges the files
the same way before deciding. And without `--force`, MCP tool calls need
approval too (`User rejected MCP: …`), retrieval included; `--allow-list`
therefore also adds `Mcp(*:optscale_kompressor_retrieve)` (`Mcp(server:tool)`,
globs, matched against the provider id). With both, a session with no
`--force` compressed the MCP result and the shell output, retrieved through
the tool, answered correctly, and uninstall restored the allow-list exactly.

Also found: a refused rewrite leaves its empty capture file behind (the hook
makes it before approval), now swept after a day; and the refusal shows the
model the rewritten command. What the desktop's approval prompt shows is still
not recorded.

### 12.7 Corner cases, three rounds (2026-11-25, CLI 2026.11.22-e9042b2)

Run on a VPS through `wrap cursor-agent` and `hook install`, each round's
findings fixed before the next.

| Case | Result |
|---|---|
| `cd`, `export` across rewritten commands; quotes, `$HOME`, a quoted heredoc, a pipe, a failing `ls` | all as bare: state carried, exit 2 kept, text intact |
| a command over 30 s (`sleep 35; seq 1 20000`) | returned inline, compressed |
| a 26 KB JSON array, then a lookup by id | answered; the retrieval had returned the **whole** 27 KB fragment — fixed, see below |
| an MCP result with an image item; an `isError` result | image kept; the error result untouched |
| MCP results of 39,000 and 45,000 bytes | the first compressed; the second spilled by Cursor before the hook — **not counted** in compressing mode, fixed |
| store unwritable; a 1 ms deadline; installed hooks whose binary cannot run | the model got the original result each time, answered correctly |
| user and project installs at once | every hook ran twice: results recorded twice, a losing rewrite's capture file left — `install` now refuses, `status` warns |
| a rival `preToolUse` rewriter in the workspace beside a user install | **the workspace hook's reply won**; ours was discarded — `install` now checks the other files Cursor reads too |
| two sessions at once on one store | both correct, no lock errors, shared content-addressed handles |
| `--sandbox enabled` | not available on that host (AppArmor); the CLI exits 1 — `wrap` no longer blames the hooks |

Fixes, in the order found: `grep` over a fragment that is a JSON array
matches elements, not the single line it is stored as (44 characters back
instead of 27,010; in `kompressor-core`, so every surface has it); spilled MCP
results are counted while compressing; `wrap` tells hooks that never ran from
hooks that ran and recorded nothing, and says nothing after an agent that
failed to start; `install` refuses a second install Cursor would also read
and a rival shell rewriter in any of those files, and `status` reports both.

Open: whether `structuredContent`, which repeats a retrieved fragment, reaches
the model — the retrieval of a 27 KB fragment came back as 63,892 characters
in Cursor's tool record.

## 13. Implementation plan

### 13.1 How the plan is laid out

Eight tiers, executed in order. A tier is the smallest piece that can be
finished, checked and left: each one ends with `main` releasable and with
something that can be verified without the tier after it.

Every tier has the same five parts:

- **Goal** — one sentence.
- **Entry gate** — what must be true before the first step. If it is not, the
  tier does not start.
- **Steps** — numbered, in the order to do them. A step is one commit or a
  small pull request.
- **Acceptance criteria** — numbered `AC-<tier>.<n>`, each one a thing that can
  be run or observed, with a yes-or-no answer. A tier is done when all of them
  are yes, and not before.
- **Exit gate** — the decision the tier's result feeds: go on, stop, or take a
  named fallback.

The order is set by one fact from §12: **the size of the saving is not known**,
because Cursor already caps what a tool result can put in the conversation. So
the measuring instrument is built first (Tiers 1–2), used (Tier 3), and only
then are the two compressing halves written (Tiers 4–5) and proven (Tier 6).

```
Tier 0 ─▶ Tier 1 ─▶ Tier 2 ─▶ Tier 3 ─┬─▶ Tier 4 ─┬─▶ Tier 6 ─▶ Tier 7
re-test   handler   install   measure │   MCP     │   prove     report,
          shadow    + wrap     (GO?)  └─▶ Tier 5 ─┘   (ON?)     docs, ship
                                          shell
```

| Tier | Name | Model sees a change | New Rust, roughly | Decision at its exit |
|---|---|---|---|---|
| 0 | Re-test on current builds | no | none | is the platform still as measured? |
| 1 | Handler in shadow mode | no | ~600 lines | — |
| 2 | Install, uninstall, wrap | no | ~700 lines | — |
| 3 | Measure real sessions | no | none | **GO / NO-GO** for Tiers 4 and 5, separately |
| 4 | MCP result compression | yes | ~300 lines | — |
| 5 | Shell output compression | yes, opt-in | ~700 lines | — |
| 6 | Prove the saving | — | none | **default ON / stay opt-in** |
| 7 | Reporting, docs, distribution | no | ~500 lines | — |

Tiers 4 and 5 are independent of each other once Tier 3 has passed; either can
go first, and either can be dropped. Sizes exclude tests and fixtures and are
estimates from the neighbouring modules (`wrap/mcp.rs` is 300 lines,
`wrap/settings.rs` 939). The open questions referred to as Q1–Q7 are the table
in §12.5.

Rules that hold in every tier:

1. `make check` is green before and after every step.
2. Nothing the user owns under `~/.cursor/` is written by a test; tests use a
   temporary home.
3. A step that needs a real Cursor session is marked **[live]**. Those are run
   by hand, cost plan usage, and are never part of `make check`.
4. If an acceptance criterion cannot be met, the tier is not "mostly done":
   the criterion is changed in this document, with the reason, or the tier is
   not finished.

### 13.2 Tier 0 — re-test on current builds

**Goal.** Confirm that everything §12 measured still holds on the Cursor builds
users have today, and turn the captures into fixtures.

**Entry gate**

- [ ] The recording hook and fixture MCP server of Appendix A are available.
- [ ] A Cursor account that can run `cursor-agent --print` sessions.

**Steps**

| # | Step | Output |
|---|---|---|
| 0.1 | Update the CLI (`agent update`) and the desktop app; record both versions | the version table at the top of this document |
| 0.2 | **[live]** Repeat the CLI sessions of §12.2 with the recording hook | §12.1's CLI column, re-stamped |
| 0.3 | **[live]** Repeat the desktop sessions of §12.3, with the fixture server enabled first | §12.1's desktop column, re-stamped |
| 0.4 | **[live]** Q2: desktop, replacement on a *successful* MCP result; the fixture's 3,000-line call for the spill threshold | §12.3 closed |
| 0.5 | **[live]** Q3: user-level `~/.cursor/hooks.json` in the desktop app; macOS if a machine is available | two rows of §12.1 filled in |
| 0.6 | Commit the captured events as `fixtures/cursor-hooks/<build>/<event>-<tool>.json`, with identifiers, paths and the e-mail address replaced | the fixtures Tier 1 parses |

**Acceptance criteria**

- AC-0.1 — Every row of §12.1 marked "proven" passes on the current CLI build
  on Linux.
- AC-0.2 — Every row of §12.1 marked "proven" for the desktop passes on the
  current desktop build.
- AC-0.3 — Q2 is answered in §12.3: yes or no, with the capture.
- AC-0.4 — `fixtures/cursor-hooks/<build>/` holds at least: `sessionStart`,
  `preToolUse` for Shell and for an MCP tool, `postToolUse` for Shell, for an
  MCP text result, for a spilled MCP result, for a failed MCP call, and for
  `Read`; and `stop` from the desktop.
- AC-0.5 — `grep -r` over the fixtures finds no e-mail address and no path
  under a real home directory.

**Exit gate**

| Result | Decision |
|---|---|
| AC-0.1 and AC-0.2 hold | go to Tier 1 |
| A "proven" row flips | stop; change the design section it supports before any code |
| Hooks do not load at all on a current build (the forum has such reports) | stop; record it here and wait for Cursor. Nothing below is worth building on a platform that is not calling it |
| macOS unavailable | go, with macOS marked untested in §12.1 and in the user guide |

### 13.3 Tier 1 — the handler, in shadow mode

**Goal.** `kompressor hook cursor` exists, parses every real event, answers
safely, and records what it *would* have removed — without changing anything
the model sees.

**Entry gate**

- [ ] Tier 0 exit gate: go.
- [ ] The fixtures of AC-0.4 are in the tree.

**Steps**

| # | Step | Where | Notes |
|---|---|---|---|
| 1.1 | `Command::Hook(HookCmd)` with subcommands `cursor` and `emit` (the latter a stub that prints its file) | `kompressor-cli/src/main.rs`, new `src/hook/mod.rs` | `emit` is declared now so Tier 5 adds no CLI surface |
| 1.2 | Event types: one `serde` struct per event used; every field optional except `hook_event_name`; unknown fields ignored | `hook/cursor.rs` | written against the fixtures |
| 1.3 | The reply path: build in memory, write once, exit 0; a panic hook that writes `{}`; an internal deadline | `hook/mod.rs` | §6.1 |
| 1.4 | `HooksConfig` / `CursorHooksConfig`: `enabled`, `deadline`, `shadow`, `mcp.*`, `shell.*` | `kompressor-core/src/config.rs` | `deny_unknown_fields`; regenerate DESIGN.md Appendix C |
| 1.5 | A statistics sink for a one-shot process, flushed before exit | `hook/mod.rs`, `kompressor-stats` | `compress` uses `null_sink()` today, so no CLI path writes `stats.db`; a 40 ms process cannot rely on the 100 ms batch timer |
| 1.6 | Shadow mode: on `postToolUse` for any tool, decode `tool_output`, run `compress_raw` without storing fragments, record one row | `hook/cursor.rs` | row: `mode = 'off'`, would-be `tokens_out`, `provider = 'cursor'`, `model`, `request_id = generation_id`. For `Read`, `content_length` only. Empty-text MCP results counted per tool |
| 1.7 | `sessionStart` returns `env` with `KOMPRESSOR_CONFIG`; no `additional_context` yet | `hook/cursor.rs` | the guidance arrives in Tier 4, with the thing it describes |

**Acceptance criteria**

- AC-1.1 — `for f in fixtures/cursor-hooks/*/*.json; do kompressor hook cursor < $f; done`
  prints one valid JSON object per fixture and every exit status is 0.
- AC-1.2 — Empty stdin, truncated JSON, a JSON array, an unknown
  `hook_event_name`, and 10 MB of garbage each produce exactly `{}` and exit 0.
- AC-1.3 — With `hooks.cursor.deadline = "1ms"`, a large fixture produces `{}`
  and exit 0, within the `timeout` an install would write (5 s).
- AC-1.4 — With `hooks.cursor.enabled = false`, every fixture produces `{}`
  and no row is written.
- AC-1.5 — In shadow mode no fixture produces `updated_input` or
  `updated_mcp_tool_output`, for any event.
- AC-1.6 — A shadow run over the `postToolUse` fixtures writes exactly one
  `optimization_events` row each, with `provider = 'cursor'` and
  `mode = 'off'`, and writes nothing to `refs.db`.
- AC-1.7 — `stats.db` and `refs.db` unreadable or on a read-only path: `{}`,
  exit 0, one line on standard error.
- AC-1.8 — Wall time for one `postToolUse` fixture of 30 KB is under 100 ms on
  the development machine (measured: `compress` alone is 40 ms).
- AC-1.9 — **[live]** With three hand-written entries in a project
  `hooks.json`, a `cursor-agent --print` session writes shadow rows, and the
  `tool_call` records of the session are identical to those of the same
  prompt run with the hooks removed, apart from ids and timings.
- AC-1.10 — `make check` is green, including the regenerated configuration
  reference.

**Exit gate.** All ten hold → Tier 2. There is no decision here; the tier
either works or is not finished.

### 13.4 Tier 2 — install, uninstall, status, and `wrap cursor-agent`

**Goal.** The hooks can be put in front of Cursor and taken away again, for
the console and the desktop, without leaving anything behind. Still shadow
only, so an install bug cannot damage a session.

**Entry gate**

- [ ] Tier 1 acceptance complete.

**Steps**

| # | Step | Where | Notes |
|---|---|---|---|
| 2.1 | Generalise claim-and-restore to an owner that is not a live process | `wrap/settings.rs` | an install is owned by a sidecar under `$KOMPRESSOR_HOME/installed/` |
| 2.2 | JSON edits of `hooks.json` and `mcp.json` that preserve everything else | new `hook/install.rs` | array-valued keys: append our entry, remove exactly it |
| 2.3 | `hook install cursor [--project] [--print] [--shadow] [--force]` | `hook/install.rs` | refuses to add a `preToolUse` entry beside another matching `Shell` without `--force` |
| 2.4 | `hook uninstall cursor` | `hook/install.rs` | removes the entries the sidecar lists; an entry edited since is reported and left |
| 2.5 | `hook status cursor` | `hook/install.rs` | what is installed and where, other hooks on the same three events, store reachable, time of the last hook call |
| 2.6 | `Wiring::Hooks { flag }` and the `cursor-agent` catalogue entry | `wrap/agents.rs` | replaces `Wiring::Unsupported` and its test |
| 2.7 | The hooks branch of the launch path: no port, no proxy, no upstreams; write the plugin directory; run; remove | `wrap/mod.rs`, `wrap/mcp.rs` | skips the plugin when an install is present (§7.1) |
| 2.8 | `wrap cursor`: replace the base-URL instructions with the explanation of §11.2 and a pointer to `hook install cursor` | `wrap/agents.rs` | and its note test |

**Acceptance criteria**

- AC-2.1 — Round trip: for each of these starting states of `hooks.json` and
  `mcp.json` — absent; empty object; the user's own hooks on other events; the
  user's own hooks on the same three events; another tool's `preToolUse` hook
  on `Shell` (with `--force`) — `install` then `uninstall` leaves both files
  byte-identical to the start, and absent files absent.
- AC-2.2 — `install` twice is the same as once; `uninstall` twice exits 0.
- AC-2.3 — An entry the user edited between `install` and `uninstall` is still
  there afterwards, and `uninstall` names it.
- AC-2.4 — `install --print` writes no file and prints JSON that
  `python3 -m json.tool` accepts.
- AC-2.5 — Without `--force`, `install` beside another `Shell` hook exits
  non-zero, writes nothing, and names the other hook.
- AC-2.6 — `wrap cursor-agent -- <args>` with a stub `cursor-agent` on `PATH`:
  the stub receives `--plugin-dir <dir>` before the user's arguments, `<dir>`
  contains `.cursor-plugin/plugin.json`, `hooks/hooks.json` and `mcp.json`
  with absolute paths, **no listener is opened**, and after the stub exits
  `<dir>` is gone.
- AC-2.7 — The same, with the wrap killed by `SIGKILL`: nothing exists outside
  `$KOMPRESSOR_HOME/run/`.
- AC-2.8 — With an install present, `wrap cursor-agent` passes no
  `--plugin-dir` and says why.
- AC-2.9 — `wrap --list` shows `cursor-agent` without "cannot be compressed",
  and `wrap cursor` prints no loopback URL.
- AC-2.10 — **[live]** On a machine with no `~/.cursor/hooks.json`:
  `hook install cursor --shadow`; one CLI session and one desktop session;
  `hook status cursor` shows a last-call time for each; shadow rows exist for
  both; `hook uninstall cursor`; `~/.cursor/` is as it was.

**Exit gate.** All ten hold → Tier 3. If AC-2.10 passes on one surface only,
go on with that surface and record the other as blocked in §12.1.

### 13.5 Tier 3 — measure real sessions (Gate A)

**Goal.** Find out, from real work, whether there is enough compressible tool
output under Cursor's 40 KB cap to justify changing what the model sees. This
tier writes no code. It answers Q5.

**Entry gate**

- [ ] Tier 2 acceptance complete.
- [ ] `hook install cursor --shadow` on at least one machine doing real work,
      ideally two or three people's.

**Steps**

| # | Step | Output |
|---|---|---|
| 3.1 | **[live]** Leave shadow mode on for a working week | at least 50 conversations, or the week, whichever is longer |
| 3.2 | Query `stats.db`: tool-output characters by tool (Shell, MCP, Read, Grep, other) | table 1 in §9.2 |
| 3.3 | For Shell and for MCP, each: share under 40 KB, and the shadow removal rate on that share | table 2 in §9.2 |
| 3.4 | Empty-text MCP results by tool; spilled shell results and the `Read` calls that follow within the same generation | table 3 in §9.2; the fronting list of §6.2 |
| 3.5 | Collect the distinct shell commands seen, de-identified, as `fixtures/cursor-shell-corpus.txt` | Tier 5's corpus |
| 3.6 | Write the numbers into §9.2 and the decision into this section | the record of the gate |

**Acceptance criteria**

- AC-3.1 — §9.2 contains the three tables, with the number of conversations,
  the date range and the Cursor builds they came from.
- AC-3.2 — For Shell and for MCP separately, one figure is stated: *the share
  of all tool-output characters that compression would have removed* (share of
  output × share under the cap × removal rate).
- AC-3.3 — `fixtures/cursor-shell-corpus.txt` exists, with at least 200
  distinct commands and no path under a real home directory.
- AC-3.4 — The decision below is written here, with the two figures beside it.

**Exit gate — GO / NO-GO, taken separately for each half**

| Figure from AC-3.2 | Decision |
|---|---|
| MCP: a clear fraction of tool output would be removed | **GO** Tier 4 |
| MCP: negligible, but empty-text results are common | **NO-GO** Tier 4; document fronting for the servers of step 3.4 instead |
| Shell: a clear fraction would be removed, or spilled results followed by reads are common | **GO** Tier 5 |
| Shell: negligible | **NO-GO** Tier 5 |
| Both negligible | **stop.** Tiers 1–2 stay as a measuring tool, §9.2 keeps its tables, the catalogue entries say what was found, and Tier 7 is reduced to documentation |

"Clear" and "negligible" are deliberately not numbers yet: there is no Cursor
baseline to derive a threshold from, and a threshold invented before the data
would be decoration. For scale, the engine removes 62.8% of eligible tool
output on the Codex corpus (README); a first-digit percentage here would not
repay Tier 5's risk. Whoever takes the decision writes the figure and the
reasoning in this section, so the threshold is on the record after the fact
even though it could not be before.

### 13.6 Tier 4 — MCP result compression

**Goal.** An MCP tool result under the spill threshold reaches the model
compressed, on both surfaces, and anything removed can be retrieved.

**Entry gate**

- [ ] Tier 3 exit gate: GO for MCP.
- [ ] Q2 answered yes (Tier 0). If it was no, this tier is CLI-only and the
      user guide says so.
- [ ] Q6 answered: the size cap on `additional_context`, by growing a string
      in a recording hook until it is dropped **[live]**.

**Steps**

| # | Step | Where | Notes |
|---|---|---|---|
| 4.1 | `postToolUse` for `MCP:*`: decode, compress each text item with `compress_raw` (`role = tool`), store fragments, reply in the shape that came in | `hook/cursor.rs` | reply only on a strict shrink |
| 4.2 | The refusals: no reply for a result that did not parse into text, for empty text, for `isError: true`, for `{}` | `hook/cursor.rs` | §12.4 |
| 4.3 | Skip our own tools and `hooks.cursor.mcp.skip_tools` | `hook/cursor.rs` | by tool name |
| 4.4 | Expose the guidance text; `sessionStart` returns it as `additional_context`, with the shell fallback line, within the Q6 cap | `kompressor-mcp/src/guidance.rs`, `hook/cursor.rs` | |
| 4.5 | Register `optscale_kompressor` in the install and in the plugin's `mcp.json`; `install` without `--shadow` now writes it | `hook/install.rs`, `wrap/mcp.rs` | never one without the other |
| 4.6 | User guide, first half: set-up, verify, "when it does not work", limits | new `docs/CURSOR.md` | in the shape of CLAUDE-DESKTOP.md |

**Acceptance criteria**

- AC-4.1 — The MCP text fixture produces `updated_mcp_tool_output` whose text
  is strictly shorter than the original and contains a `ref:k_…` handle.
- AC-4.2 — `kompressor retrieve <that handle>`, run from a different working
  directory with the same `KOMPRESSOR_CONFIG`, prints the elided text.
- AC-4.3 — A result with a text item and an image item: the image item is
  byte-identical in the reply.
- AC-4.4 — The fixtures for a failed call (`{}`), an error result, a spilled
  result (empty text), and a result shorter than `min_compress_chars` each
  produce `{}`.
- AC-4.5 — A result from `MCP:optscale_kompressor_retrieve` produces `{}`.
- AC-4.6 — A result the engine cannot shrink produces `{}` — never a reply of
  equal or greater length.
- AC-4.7 — Store unwritable: `{}`, and the session is unaffected.
- AC-4.8 — `hook install cursor` (no `--shadow`) followed by `uninstall`
  still meets AC-2.1, now including the `optscale_kompressor` server entry.
- AC-4.9 — **[live]** CLI: against the fixture server, a 30 KB result reaches
  the model compressed (seen in the `tool_call` record), and the model answers
  a question whose answer is only inside the elided part — once by calling the
  retrieve tool, once, with MCP disabled, by running `kompressor retrieve`.
- AC-4.10 — **[live]** Desktop: the same, with the tool.
- AC-4.11 — **[live]** With the binary made unexecutable mid-session, the next
  MCP call returns its original result and the session continues.

**Exit gate.** All hold → the MCP half is complete and ships **opt-in**
(`hook install cursor`), not on by default. Tier 6 decides the default. If
AC-4.10 fails while AC-4.9 passes, ship CLI-only and document fronting for
the desktop.

### 13.7 Tier 5 — shell output compression

**Goal.** A shell command's output is compressed before Cursor captures it,
without changing what the command does, its exit status, or the user's
approval decisions.

**Entry gate**

- [ ] Tier 3 exit gate: GO for Shell.
- [ ] `fixtures/cursor-shell-corpus.txt` from step 3.5.
- [ ] Q1 answered **[live]**: in the brace form, an executable inside the group
      that is not allow-listed is still rejected; and what the desktop
      approval prompt shows for a rewritten command is recorded in §12.3.
- [ ] Q4 answered **[live]**: behaviour under a sandbox policy that denies
      writes outside the workspace.

If Q1 is answered *no* — the brace form lets an unapproved command through —
the tier still proceeds, but step 5.6 becomes mandatory and the rewrite is
permanently restricted to sessions that already run everything.

**Steps**

| # | Step | Where | Notes |
|---|---|---|---|
| 5.1 | The tokenizer: quotes, escapes, heredoc starts, control operators, balance | new `hook/shell.rs` | no shell-parser dependency |
| 5.2 | The classifier, one function per row of §6.3's "not rewritten" table | `hook/shell.rs` | "cannot decide" is "do not rewrite" |
| 5.3 | The rewrite: create the capture file, emit the brace form with single-quoted absolute paths | `hook/shell.rs` | the hook creates the file, so an unwritable `$TMPDIR` is caught before the rewrite |
| 5.4 | `hook emit --status N FILE`: compress, print, delete, exit `N`; on any failure print the file unchanged; `max_capture_bytes` | `hook/mod.rs` | exit status is `N` on every path that printed |
| 5.5 | The lossless fallback when the store is unreachable | `hook/mod.rs` | §8 |
| 5.6 | Detect the approval mode (`approvalMode` in `~/.cursor/cli-config.json`) and decline to rewrite outside run-everything, if Q1 requires it | `hook/shell.rs` | only if Q1 was *no* |
| 5.7 | `install`: offer `Shell(kompressor)` for the CLI allow-list | `hook/install.rs` | asked, never assumed |
| 5.8 | `hooks.cursor.shell.enabled` defaults to `false`; user guide, second half | config, `docs/CURSOR.md` | including what a rewritten command looks like in a terminal file |

**Acceptance criteria**

- AC-5.1 — One unit test per row of §6.3's "not rewritten" table, each with a
  command that must be left alone and a near miss that must be rewritten.
- AC-5.2 — For every command in `fixtures/cursor-shell-corpus.txt`: the
  classifier terminates, and where it rewrites, the result is accepted by
  `sh -n`, `bash -n` and `zsh -n`.
- AC-5.3 — Equivalence, under `sh`, `bash`, `zsh` and `dash`, for a fixed set
  of at least 40 commands run in a scratch directory with compression
  disabled: standard output and standard error combined are byte-identical to
  the bare command, and the exit status reaching `emit --status` equals the
  bare command's.
- AC-5.4 — State: a rewritten `cd DIR` and a rewritten `export X=1` are both
  visible to the next command in the same shell, in `bash` and `zsh`.
- AC-5.5 — Commands ending in a heredoc, a comment, and a line continuation
  each either rewrite to a form that passes AC-5.3 or are left alone.
- AC-5.6 — `hook emit` on a missing file, an unreadable file, a file over
  `max_capture_bytes`, and with the store unwritable: prints what it can
  unchanged, exits `N`, never exits 0 for a non-zero `N`.
- AC-5.7 — After `hook emit`, the capture file does not exist. After a
  `SIGKILL` between the command and `emit`, it does, and its path is under
  `$TMPDIR` with mode 0600.
- AC-5.8 — The rewritten text contains no `(`, no `||`, no `$` other than
  `$?`, and no executable other than the user's command and the absolute path
  of this binary. Asserted over the corpus.
- AC-5.9 — No reply ever contains `permission`. Asserted over the corpus.
- AC-5.10 — **[live]** CLI with `Shell(<cmd>)` and `Shell(kompressor)`
  allow-listed and no `--force`: a rewritten allow-listed command runs; a
  rewritten command that is not allow-listed is rejected.
- AC-5.11 — **[live]** CLI and desktop: `seq 1 20000` returns inline, under
  1 KB, with a handle the model retrieves from; a failing command reports its
  own exit status; a command longer than 30 seconds returns its output.
- AC-5.12 — **[live]** One working week of the implementer's own sessions with
  the rewrite on, with every command that behaved differently because of it
  recorded. The criterion is that the list is empty; one entry reopens the
  tier.

**Exit gate.** All hold → the shell half is complete and ships **off by
default**. Tier 6 decides whether that changes. If AC-5.10 cannot be met, the
shell half ships restricted to run-everything sessions (step 5.6), and that
restriction is stated in the user guide's first paragraph on the subject.

### 13.8 Tier 6 — prove the saving (Gate B)

**Goal.** Establish, on Cursor's own accounting, whether hook mode lowers
usage without lowering the quality of the work. This is the only evidence
that may be used to turn either half on by default or to put a number in the
README.

**Entry gate**

- [ ] At least one of Tier 4 and Tier 5 complete.
- [ ] A fixed task set runnable through `cursor-agent --print`, with a checker
      — the Terminal-Bench harness of BENCHMARK.md, with the agent swapped, is
      the intended one.
- [ ] A way to read billed usage per run from Cursor: the usage export, or the
      desktop `stop` event for desktop runs (§9.3).

**Steps**

| # | Step | Output |
|---|---|---|
| 6.1 | Three arms: hooks off; MCP half on; both halves on (or whichever exist) | the run matrix |
| 6.2 | **[live]** Run every arm over the task set, at least three times each | per-task usage and outcome |
| 6.3 | Diff the `tool_call` records between arms for commands whose result differed other than by compression | the "behaved differently" list |
| 6.4 | Read retrievals and their hit rate from `stats.db` | the cost of recoverability |
| 6.5 | Write the tables into BENCHMARK.md and the headline into the README | the published result |

**Acceptance criteria**

- AC-6.1 — Billed usage per full run, per arm, with the run-to-run range, is
  in BENCHMARK.md, stating its source and the Cursor builds used.
- AC-6.2 — Tasks solved per arm, with the same range.
- AC-6.3 — The "behaved differently" list of step 6.3 is published, empty or
  not.
- AC-6.4 — Every retrieval in the on-arms was a hit.
- AC-6.5 — The write-up says, as the Claude Code one does, which tasks account
  for the difference and how much of it is path variance rather than
  mechanism.

**Exit gate — per half**

| Result | Decision |
|---|---|
| Usage lower outside the run-to-run range, solve rate inside it, AC-6.3 empty | the half becomes the default for `hook install cursor`; the number goes in the README |
| Usage lower, but AC-6.3 not empty | stays opt-in; the list reopens Tier 5 |
| No difference outside the noise | stays opt-in; the README says what was measured and that it showed no saving |
| Solve rate lower outside its range | stays opt-in and is marked experimental; investigate before any further work |

### 13.9 Tier 7 — reporting, documentation, distribution

**Goal.** Cursor sessions are first-class in the statistics and the
documentation, and installing is as easy as it can be made.

**Entry gate**

- [ ] Tier 6 complete, or Tier 3 ended in "stop" — in which case only steps
      7.4 and 7.5 apply.

**Steps**

| # | Step | Where |
|---|---|---|
| 7.1 | `Surface::Hook` with its table-rebuild migration; `conversation` on optimization events | `kompressor-stats` |
| 7.2 | Record the desktop `stop` event's usage as a `proxy_requests`-shaped row | `hook/cursor.rs`, `kompressor-stats` |
| 7.3 | A Cursor section in the dashboards: removal by tool; usage per conversation where `stop` supplied it | `kompressor-server` |
| 7.4 | DESIGN.md §2.1 and §14.1, README *Other agents*, QUICKSTART's `wrap --list` sample | docs |
| 7.5 | This file rewritten as "as built", like its siblings in `docs/design/` | docs |
| 7.6 | A published Cursor plugin; a PowerShell form of the rewrite | separate work, each with its own design note |

**Acceptance criteria**

- AC-7.1 — The migration upgrades a copy of a real `stats.db` and
  `kompressor db check` passes on it; existing rows keep their `surface`.
- AC-7.2 — A desktop session produces one usage row per turn, and
  `v_request_economy` returns rows for `provider = 'cursor'`.
- AC-7.3 — The dashboard's Cursor section renders with real data and with
  none.
- AC-7.4 — `scripts/docs/check.py` passes: every documented command exists,
  the configuration reference matches, every `§` resolves.
- AC-7.5 — No sentence in README, QUICKSTART or DESIGN.md still says the
  Cursor CLI cannot be compressed, and none promises a saving Tier 6 did not
  measure.
- AC-7.6 — This file carries no **[unverified]** tag.

**Exit gate.** All hold → the feature is released and announced in the
changelog. Step 7.6 is not part of the gate.

### 13.10 Risks

| Risk | Likelihood | What limits it |
|---|---|---|
| The saving is too small to matter, because of the 40 KB cap | real | Tier 3's gate, before either compressing half is written |
| A Cursor release changes a payload or stops firing a hook | real — the forum record shows it has | fixtures named by build (Tier 0); unknown shapes answer `{}`; `hook status` shows the last call, so silence is visible |
| A rewritten command behaves differently from the original | moderate | the classifier's bias to not rewrite; the corpus test; off by default until Tier 6; one switch to disable |
| The rewrite weakens command approval | low if the form is kept | no form that hides `C` (§6.3); Q1 before Tier 5; never return `permission` |
| A hook replaces an error with text | low | reply only to parsed, non-error, non-empty text (§12.4) |
| Two tools rewriting the same command | moderate, RTK is popular | `install` refuses beside another `Shell` hook; `status` lists them |
| Residue left in `~/.cursor/` | low | sidecar ownership; the round-trip tests of Tier 2 (AC-2.1); `wrap` leaves none |
| Managed machines disable user hooks | unknown | `install --print` for the enterprise path; fronting needs no hooks |

### 13.11 Test levels

| Level | What |
|---|---|
| unit | event parsing for every captured payload, including the shapes not expected; reply shaping; the shell classifier, one test per row of §6.3; the rewritten form generated for a command ending in a heredoc, a comment, a line continuation |
| integration | `hook cursor` driven over stdin with recorded events against a temporary store: compressed reply, fragment retrievable, stats row written; store unwritable → `{}`; deadline exceeded → `{}` |
| shell | the rewritten form executed under `sh`, `bash`, `zsh` and `dash`: exit status, `cd` persistence, output equality with compression off |
| install | `install` → `uninstall` round-trips arbitrary pre-existing `hooks.json` and `mcp.json`, including one with the user's own hooks on the same events |
| end to end | scripted `cursor-agent --print` sessions with a fixture MCP server returning a known large payload; gated behind a credential, not part of `make check` |

Recorded events live in `fixtures/cursor-hooks/`, one directory per build they
were captured on, so a new Cursor build that changes a payload shows up as a
diff in a fixture rather than as a silent `{}`.

### 13.12 What was built, by tier

| Tier | Built | Verified by | Open |
|---|---|---|---|
| 0 | fixtures in `fixtures/cursor-hooks/2026.09.26-dd393fe/`, reconstructed from Appendix B (no raw capture was taken in the spikes) | AC-0.4, AC-0.5 (`tests/cursor.rs`) | 0.1–0.5 **[live]**; CLI 2026.11.22-e9042b2 is now installed and untested |
| 1 | `hook cursor`: one reply, exit 0, panic hook, deadline thread; `[hooks.cursor]`; one-shot stats sink; shadow rows (`mode = 'off'`, `tool`, `conversation`) | AC-1.1–1.7 in `tests/cursor.rs`; AC-1.8: 0.6 s for the first call on a 400 MB v17 database (migration included), well under the deadline after | AC-1.9 **[live]** |
| 2 | `hook install / uninstall / status cursor` with byte-for-byte restore; `Wiring::Hooks`; `wrap cursor-agent` with `--plugin-dir`; `wrap cursor` re-pointed | AC-2.1–2.6, 2.8, 2.9 | AC-2.7 (SIGKILL) by inspection: only `run/` is written; AC-2.10 **[live]** |
| 3 | `stats by-tool`, the Cursor dashboard page: the instruments Gate A reads | — | the whole tier **[live]** |
| 4 | MCP compression mirroring the result's shape; refusals for `{}`, `isError`, empty text, our own tools, `skip_tools`; `session_guidance()` | AC-4.1–4.8 | AC-4.9–4.11 **[live]**; Q6 (the `additional_context` cap) open — the guidance is ~3 KB |
| 5 | tokenizer and classifier (`hook/shell.rs`), the brace-form rewrite, `hook emit`, lossless fallback, `only_when_approved` (step 5.6, taken because Q1 is open), `--allow-list` | AC-5.1, 5.3 (40 commands × sh, bash, dash; zsh where installed), 5.4–5.9 | AC-5.2 needs the Tier 3 corpus; AC-5.10–5.12 **[live]** |
| 6 | — | — | the whole tier **[live]** |
| 7 | `Surface::Hook` and the v20 rebuild (keys and ids preserved, `foreign_keys` off around the ladder); `stop` usage as a `proxy_requests` row; dashboard page; DESIGN, README, QUICKSTART, docs/CURSOR.md | AC-7.1 on a copy of a real 400 MB v17 `stats.db` (`db check` ok, every row kept its surface); AC-7.2 in `tests/cursor.rs`; AC-7.3 data half | AC-7.3 page rendered by eye; AC-7.6 waits on the live tiers; 7.6 is separate work |

Two things differ from the plan as written:

- `hook install` writes a fourth entry, `stop`, for step 7.2's usage rows.
- Removal is counted only against what would have been inline. The first live
  run reported `seq 1 20000` (109 KB) as ~78k tokens removed, but without
  compression Cursor would have written it to a file and the model would never
  have read it inline (§3.5). For a result over 40,000 bytes the row's
  `tokens_in` is set to its `tokens_out`, `chars_in` keeps the real size, and
  `stats by-tool` counts it under *kept inline* instead.
- Shell rewriting refuses `exit`, `exec`, `set` and `trap` as well: inside the
  brace group they would end Cursor's shell, or change how it treats errors,
  before `emit` could print the output.

## 14. Changes to existing code and docs

| Where | Change |
|---|---|
| `crates/kompressor-cli/src/hook/` | new: `mod.rs` (subcommands), `cursor.rs` (event ↔ engine translation), `shell.rs` (classifier and rewrite), `install.rs` |
| `crates/kompressor-cli/src/main.rs` | `Command::Hook(HookCmd)` |
| `crates/kompressor-cli/src/wrap/agents.rs` | `Wiring::Hooks { flag }`; the `cursor-agent` entry changes from `Unsupported` to it; the test `the_cursor_cli_is_listed_as_incompatible_rather_than_omitted` is replaced by one asserting no proxy is started for it |
| `crates/kompressor-cli/src/wrap/mod.rs` | the launch path gains a branch that skips proxy start-up and upstream mounting |
| `crates/kompressor-cli/src/wrap/settings.rs` | claim-and-restore generalised to a claim with no owning process, for `hook install` |
| `crates/kompressor-core/src/config.rs` | `HooksConfig` / `CursorHooksConfig`; the configuration reference in DESIGN.md Appendix C must be regenerated, or `scripts/docs/check.py` fails |
| `crates/kompressor-mcp/src/guidance.rs` | expose the guidance text for `sessionStart` |
| `crates/kompressor-stats` | Tier 1: a flushed sink for a one-shot process. Tier 7: `Surface::Hook`, migration, `conversation` on optimization events |
| `docs/CURSOR.md` | new user guide, in the shape of CLAUDE-DESKTOP.md: set-up for console and desktop, verify, "when it does not work", limits |
| `docs/DESIGN.md` §2.1, §14.1 | a "Hook mode" row in the surface table; the Cursor paragraphs rewritten to describe what was built; the protobuf finding kept, as the reason for it |
| `README.md`, `docs/QUICKSTART.md` | the *Other agents* paragraph and the `wrap --list` sample |
| this file | becomes "as built" — the tags replaced by measurements, like its siblings in `docs/design/` |

The engine, the store, the wire crate and the proxy are not touched.

## 15. References

Fetched 2026-11-25. **[docs]** tags refer to the first list, **[forum]** to the
second.

Cursor documentation:

- Hooks — <https://cursor.com/docs/agent/hooks>
- Claude Code hook compatibility — <https://cursor.com/docs/reference/third-party-hooks>
- MCP — <https://cursor.com/docs/context/mcp>
- CLI — <https://cursor.com/docs/cli/using>,
  <https://cursor.com/docs/cli/reference/parameters>,
  <https://cursor.com/docs/cli/reference/configuration>
- API keys and the base-URL override — <https://cursor.com/docs/settings/api-keys>
- Network hosts, HTTP/2 and proxies — <https://cursor.com/docs/enterprise/network-configuration>
- Pricing — <https://cursor.com/docs/account/pricing>
- Terms — <https://cursor.com/terms-of-service>, <https://cursor.com/acceptable-use-policy>

Cursor forum (staff answers unless marked):

- `after*` hooks are observation-only; `postToolUse` has no MCP server identity —
  <https://forum.cursor.com/t/aftermcpexecution-return-value-is-discarded-and-posttooluse-carries-no-mcp-server-identity/167261>
- Which hooks the CLI fires —
  <https://forum.cursor.com/t/cursor-cli-doesnt-send-all-events-defined-in-hooks/148316>
- User-level `hooks.json` not loaded by the CLI on macOS, open —
  <https://forum.cursor.com/t/cursor-cli-2026-11-05-macos-user-level-cursor-hooks-json-is-never-loaded-project-level-cursor-hooks-json-works/177989>
- No hooks in the CLI on Linux at 2026.08.11, user report, open —
  <https://forum.cursor.com/t/cursor-agent-cli-never-invokes-hooks-json-on-linux-2026-08-11-same-version-works-on-macos/168326>
- Imported Claude Code hooks in the CLI —
  <https://forum.cursor.com/t/cursor-agent-imported-claude-code-plugin-hooks-get-a-cursor-shaped-payload-so-pretooluse-gates-fail-open/173171>
- Base-URL override is called from Cursor's servers —
  <https://forum.cursor.com/t/connecting-local-ai-server-to-cursor-does-not-work/152940/3>
- Agent mode sends Responses to the override —
  <https://forum.cursor.com/t/unable-to-use-custom-openai-compatible-api/153988>
- No bring-your-own-key in the CLI —
  <https://forum.cursor.com/t/can-i-use-provider-api-keys-with-cursor-cli-agent/149158>
- Private endpoints and the terms —
  <https://forum.cursor.com/t/does-using-oh-my-pi-s-cursor-provider-or-an-openai-compatible-proxy-to-the-same-endpoints-violate-cursor-s-tos/167778>

Prior art:

- RTK's Cursor hook — <https://github.com/rtk-ai/rtk/pull/595>
- lean-ctx's Cursor hooks — <https://docs.rs/crate/lean-ctx/3.5.7/source/src/hooks/agents/cursor.rs>
- LiteLLM's relay for the CLI — <https://docs.litellm.ai/docs/tutorials/cursor_cli_subscription>

In this repository:

- [DESIGN.md §14.1](../DESIGN.md#141-wrap-running-an-agent-behind-the-proxy) —
  `wrap`, the agent catalogue, the original Cursor findings
- [design/MCP-PROXY.md](MCP-PROXY.md) — why tool results are the right thing to
  compress when history is server-held
- [design/INTERCEPT.md](INTERCEPT.md) — the listener §11.1 would have used
- [CLAUDE-CODE.md](../CLAUDE-CODE.md) — the session-scoped MCP offer this design
  copies for `--plugin-dir`

## Appendix A: how the measurements were taken

All on the builds named at the top. The CLI is a Node bundle under
`~/.local/share/cursor-agent/versions/<build>/`; the desktop app is under
`/usr/share/cursor/resources/app/`. "Read out of the build" means a fixed-string
search of those files — it shows that code handling a field exists, not that
every path reaches it, which is why the spikes in §12 exist.

| Claim | How |
|---|---|
| Hook events in the CLI | `grep -o -E '_E\.[a-zA-Z]+' 190.index.js \| sort -u` in the build directory |
| `updated_mcp_tool_output` is MCP-only | the one use is in the MCP executor, immediately after `firePostToolUse`; accepts a string or `{content:[…]}` |
| `updated_input` per tool | each tool's `applyUpdatedInput` function in the same file; Shell assigns `command` and `cwd`, `Write` assigns `file_path` only |
| no output replacement for other tools | `grep -c` for `updated_tool_output` and `updated_output` returns 0 in both builds |
| hook file locations | the path-building function beside `enterpriseConfigPath` / `userConfigPath` / `projectConfigPath` / `claudeUserConfigPath` |
| fail-open default | `hasFailClosedHooksForStep` is consulted only when an entry sets `failClosed` |
| `postToolUse` fields | the object literal passed to `executeHookForStep(postToolUse, …)` |
| desktop carries the same fields | `grep -rl updated_mcp_tool_output /usr/share/cursor/resources/app` → three files under `extensions/cursor-agent-*` |
| `--plugin-dir` is public | `cursor-agent --help` |
| plugin layout | `~/.cursor/plugins/cache/cursor-public/gmail/<hash>/.cursor-plugin/plugin.json` |
| local-provider mode | option definitions in `index.js`; `CURSOR_AGENT_CLI_LOCAL_MODE=true cursor-agent --help` prints `Usage: agent-local` and still omits `--base-url` |
| hook process cost | `/usr/bin/time kompressor compress big.log --config /dev/null`, 4,000 lines: 0.04 s, 15 MB, second and third runs |

### The spikes

A scratch workspace with `.cursor/hooks.json` and `.cursor/mcp.json`; a
recording hook that appends each event, with its environment and working
directory, to a JSONL file and answers from a JSON table (fixed replies per
event, and a map from exact command to rewritten command); a 40-line stdio MCP
server with one tool returning *N* log lines. Each session was

```
cursor-agent -p --trust --output-format stream-json [--force] [--approve-mcps] [--sandbox enabled] [--plugin-dir DIR] "<prompt naming the exact calls to make>"
```

and was read two ways: the `tool_call` records in the stream, which show what
Cursor ran and returned, and the hook log, which shows what the hooks were
given. The model's own account was used only for what it alone can report —
what it believes it ran, and whether injected context reached it.

For the user-level source a `~/.cursor/hooks.json` was created for one session
and removed; none existed before. The compressing runs used
`kompressor compress FILE` with a throwaway configuration in place of the
`hook emit` this design proposes, so they exercise the shell form and the
engine, not the subcommand.

## Appendix B: hook payloads

Captured on CLI 2026.09.26-dd393fe, trimmed; identifiers and the e-mail
address removed.

**Common to every event:** `conversation_id`, `session_id`, `generation_id`,
`model`, `hook_event_name`, `cursor_version`, `workspace_roots`,
`transcript_path`, `user_email`. Environment: `CURSOR_PROJECT_DIR`,
`CURSOR_VERSION`, `CURSOR_USER_EMAIL`, `CLAUDE_PROJECT_DIR`, and
`CURSOR_PLUGIN_ROOT` for a plugin hook. Working directory: the workspace for a
project hook, `~/.cursor` for a user hook, the plugin directory for a plugin
hook — so nothing in the handler may depend on it.

**`sessionStart` — in** adds `is_background_agent`. **Out:**

```json
{"additional_context": "…", "env": {"KOMPRESSOR_CONFIG": "/abs/path/config.toml"}}
```

**`preToolUse`, Shell — in:**

```json
{
  "hook_event_name": "preToolUse",
  "tool_name": "Shell",
  "tool_input": {"command": "seq 1 30", "cwd": "", "timeout": 30000},
  "tool_use_id": "…",
  "cwd": ""
}
```

`cwd` was empty in every capture; the documentation spells the key
`working_directory`. The handler reads and returns `command` only.

**`preToolUse` — out, when the command is eligible:**

```json
{"updated_input": {"command": "{\nseq 1 30\n} > '/tmp/kompressor-<id>.out' 2>&1; '/usr/bin/kompressor' hook emit --status \"$?\" '/tmp/kompressor-<id>.out'"}}
```

**`postToolUse`, Shell — in.** `tool_input.command` is the *rewritten* command
when one was applied:

```json
{
  "tool_name": "Shell",
  "tool_input": {"command": "seq 1 30", "cwd": "", "timeout": 30000},
  "tool_output": "{\"output\":\"1\\n2\\n…30\\n\",\"exitCode\":0}",
  "duration": 915.327
}
```

**`postToolUse`, MCP — in:**

```json
{
  "tool_name": "MCP:big_text",
  "tool_input": {"lines": 40},
  "tool_output": "{\"content\":[{\"type\":\"text\",\"text\":\"Compiling crate-1 …\"}],\"isError\":false}",
  "duration": 3.656
}
```

and for a result Cursor spilled to a file:

```json
{"tool_name": "MCP:big_text", "tool_input": {"lines": 3000},
 "tool_output": "{\"content\":[{\"type\":\"text\",\"text\":\"\"}],\"isError\":false}"}
```

**`postToolUse` — out, when the engine shrank the result:**

```json
{"updated_mcp_tool_output": {"content": [{"type": "text", "text": "…compressed…"}]}}
```

**`afterMCPExecution` — in**, the only place the server is named; here
`tool_name` has no `MCP:` prefix and `tool_input` is a string:

```json
{"tool_name": "big_text", "mcp_server_name": "kspike", "tool_input": "{\"lines\":40}", "result_json": "{…}"}
```

**`postToolUse`, Read — in**, no content:

```json
{"tool_name": "Read", "tool_input": {"file_path": "…"}, "tool_output": "{\"file_path\":\"…\",\"content_length\":276}"}
```

**Every other case — out:**

```json
{}
```
