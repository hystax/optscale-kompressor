# Cursor integration

Compress what Cursor's agent reads from its tools, in the editor and in the
console (`cursor-agent`), through Cursor's own hooks. No proxy runs, nothing
intercepts Cursor's traffic, and your Cursor login and plan are unchanged.

```text
Cursor agent ── a tool returns ──▶ postToolUse hook ──▶ kompressor hook cursor
                                                         │ engine + refs.db + stats.db
             ◀── the result, compressed ─────────────────┘
```

Built from [design/CURSOR.md](design/CURSOR.md), which has the measurements
behind every statement here. Measured on Linux against Cursor CLI
2026.09.26-dd393fe and Cursor desktop 3.28.7. **Not yet measured:** macOS,
current Cursor builds, and whether the saving is worth having on real work —
see [Limits](#limits).

## Why hooks, and not the proxy

Cursor's agent talks to Cursor's own servers over a private protobuf protocol,
and the prompt is assembled on those servers, so a proxy has nothing it can
read. Its "Override OpenAI Base URL" setting is called from Cursor's servers
too, so it never reaches an address on your machine. What Cursor does offer is
**hooks**: a command it runs with each tool result, before the model sees it,
whose answer can replace the result. A result compressed there enters the
conversation once, smaller, and stays that way for every later turn.

| Tool output | What happens |
|---|---|
| MCP tool results | compressed, replaced through `updated_mcp_tool_output` |
| Shell output | compressed, **off by default**: the command is rewritten so its output is compressed before Cursor captures it ([below](#shell-output)) |
| MCP results over ~40 KB | Cursor writes them to a file first and the hook sees nothing; front that server with [`[[mcp.servers]]`](MCP-PROXY.md) instead |
| Built-in Read, Grep, List, Fetch | **not compressible**: no hook can change them |

Anything removed sits behind a `ref:k_…` handle the model can read back, with
the `optscale_kompressor_retrieve` tool or `kompressor retrieve` in the shell.

## Step by step

### 0. Prerequisites

- Kompressor 2.6 or later on the machine Cursor runs on
  ([QUICKSTART](QUICKSTART.md) has the packages). Check it:

  ```bash
  kompressor version
  ```

- Cursor desktop, and/or the Cursor CLI logged in (`cursor-agent status` says
  *Logged in*). A subscription is fine: nothing here needs an API key.
- `~/.cursor/hooks.json` either absent or without another tool's `preToolUse`
  hook on `Shell` (RTK, lean-ctx). `kompressor hook status cursor` lists any.

### 1. Give Kompressor a configuration with absolute paths

The hooks, the shell step and the retrieval server run from different
directories, so the fragment store must be named absolutely. Put this in
`~/.config/kompressor/config.toml` (any path works; pass it with `--config`):

```toml
[refs]
path = "/home/you/.local/share/kompressor/refs.db"

[stats]
path = "/home/you/.local/share/kompressor/stats.db"
```

Without a file, `hook install` writes a pinned copy under
`~/.kompressor/installed/` with the store in `~/.kompressor/data/`, which also
works; the file just makes the settings below editable in one place.

### 2a. Console only, one session at a time

Nothing is installed; the hooks live for one session:

```bash
kompressor wrap cursor-agent                              # interactive
kompressor wrap cursor-agent -- -p --approve-mcps "…"     # headless
```

Approve the `kompressor` plugin's MCP server when asked (or pass
`--approve-mcps`), so the model can call `optscale_kompressor_retrieve`. On
exit `wrap` prints how many tool results the hooks saw and roughly how many
tokens were removed. Done — skip to step 5 to verify.

### 2b. Desktop, and every CLI session: install the hooks

Start by measuring, which changes nothing the model sees:

```bash
kompressor hook install cursor --shadow
```

Use Cursor normally for a while (a day, or a few real tasks), then look at what
compression would have removed:

```bash
kompressor stats by-tool
```

When that is worth having, install for real — this replaces the shadow install:

```bash
kompressor hook install cursor
```

### 3. Desktop: enable the retrieval server and start a new chat

1. Cursor reloads `~/.cursor/hooks.json` by itself; no restart is needed.
2. Open Cursor Settings → **MCP** (Tools & Integrations) and make sure
   **optscale_kompressor** is enabled. It is how the model reads back what was
   removed. Without it the model can still run the `kompressor retrieve`
   command it is told about at session start.
3. Start a **new** Agent chat: the session-start hook, which tells the model how
   markers and retrieval work, runs only when a session begins.

### 4. CLI after an install

Nothing to wrap: every `cursor-agent` session reads `~/.cursor/hooks.json`.
Approve `optscale_kompressor` when the CLI asks, or pass `--approve-mcps`. Do
not also use `wrap` on this machine — it notices the install and adds nothing.

### 5. Verify

```bash
kompressor hook status cursor
```

Expect `installed yes`, every entry `ok`, `store … (reachable)`, and after one
session, `last call` with a time. Then ask the agent for something that
produces a large MCP result and look for a marker like
`[×280 similar ref:k_…]` in it; `kompressor stats by-tool` should count the
result, and the dashboard (`kompressor serve`, then `/cursor`) shows the same.

### 6. Optional: shell output

Shell output is off by default. To turn it on, add to the configuration:

```toml
[hooks.cursor.shell]
enabled = true
```

It then rewrites a command only when the Cursor CLI would run the rewritten
one: its `approvalMode` is `unrestricted` (run everything), or its allow-list
approves `kompressor`. With an allow-list, add the entries — this adds
`Shell(kompressor)`, `Shell(<absolute path of the binary>)` and
`Mcp(*:optscale_kompressor_retrieve)` (so the model may read back what was
removed) to `~/.cursor/cli-config.json`:

```bash
kompressor hook install cursor --allow-list
```

The rewrite does not let anything through that you had not approved: an
unapproved command inside it is still refused ([Shell output](#shell-output)).
A workspace `.cursor/cli.json` that sets `permissions.allow` **replaces** the
global allow-list for that workspace (the CLI merges objects, not lists), so
there the entries have to be in that file too; the hook reads both the same
way and stays idle when the merged list lacks them.
The desktop's approval settings cannot be read from outside, so in the editor
set `only_when_approved = false` for the rewrite to run.

### 7. Turn it off, or remove it

```toml
[hooks.cursor]
enabled = false        # every hook answers {} from the next tool call on
```

```bash
kompressor hook uninstall cursor   # removes exactly what install wrote
```

## Console: `kompressor wrap cursor-agent`

```bash
kompressor wrap cursor-agent -- -p "fix the failing test"
```

This writes a plugin directory under `~/.kompressor/run/`, starts
`cursor-agent --plugin-dir <that directory> <your arguments>`, and removes the
directory when the agent exits. The plugin carries the hooks and the retrieval
server; nothing in `~/.cursor` is touched, and no proxy is started. The agent's
exit status is the command's.

If the hooks are already installed ([below](#editor-and-every-session-kompressor-hook-install-cursor)),
`wrap` adds nothing and says so: the CLI reads `~/.cursor/hooks.json` too, and
two sets would run every hook twice.

## Editor, and every session: `kompressor hook install cursor`

The editor is not started by `kompressor`, so the hooks have to be registered
where it reads them:

```bash
kompressor hook install cursor
```

| Writes | What |
|---|---|
| `~/.cursor/hooks.json` | four entries: `sessionStart`, `preToolUse` on `Shell`, `postToolUse` on `MCP:`, `stop` |
| `~/.cursor/mcp.json` | one server, `optscale_kompressor` (`kompressor mcp serve`) |
| `~/.kompressor/installed/cursor.json` | what was written, so uninstall removes exactly that |

Cursor reloads `hooks.json` on its own. In the editor, enable the
`optscale_kompressor` server under Settings → MCP if it is not enabled for you.

Options:

| Flag | Effect |
|---|---|
| `--shadow` | measure only: every tool's output is run through the engine and what would have been removed is recorded; nothing the model sees changes, and no MCP server is registered. Start here. |
| `--project` | install into `<workspace>/.cursor` instead of `~/.cursor` |
| `--config PATH` | the configuration the hooks run under |
| `--print` | print the entries as JSON and write nothing — for `/etc/cursor/hooks.json` on managed machines |
| `--force` | install beside another tool's `preToolUse` hook on `Shell` (RTK, lean-ctx). Without it, install refuses: two hooks rewriting one command overwrite each other |
| `--allow-list` | add `Shell(kompressor)`, `Shell(<absolute path>)` and `Mcp(*:optscale_kompressor_retrieve)` to the CLI's allow-list in `~/.cursor/cli-config.json`; uninstall removes exactly what it added |

Remove it again, exactly:

```bash
kompressor hook uninstall cursor
```

A file nobody has touched since is put back byte for byte, and one the install
created is removed. An entry you edited in the meantime is reported and left.

### Which configuration the hooks use

The hook, the shell's `emit` step and the retrieval server are three processes
started from three directories, so the fragment store has to be named
absolutely. If your configuration file already names `refs.path` and
`stats.path` absolutely, the hooks use that file, and edits to it apply at the
next tool call. Otherwise the installer writes a copy with the paths made
absolute to `~/.kompressor/installed/cursor.toml`; with no configuration at
all, the store goes in `~/.kompressor/data/`. Every command it writes names
the file with `--config`.

```toml
[hooks.cursor]
# Master switch. The installed hooks answer {} when this is false, so
# compression stops without editing a file Cursor owns.
enabled = true
# Give up and change nothing after this long; must stay below the 5 s timeout
# written into hooks.json.
deadline = "2s"
# Record what would be removed, without changing anything.
shadow = false

[hooks.cursor.mcp]
enabled = true
# Tools whose results are forwarded untouched, by name (the hook is not told
# which server a tool belongs to).
skip_tools = []

[hooks.cursor.shell]
enabled = false
only_when_approved = true
skip = ["cd", "pwd", "echo", "export", "mkdir", "touch", "rm", "mv", "cp", "chmod", "which", "git add", "git commit", "git checkout", "git switch", "git push"]
max_capture_bytes = 67108864
```

Each also reads from the environment Cursor starts hooks with, as
`KOMPRESSOR__HOOKS__CURSOR__ENABLED=false` and so on.

## Shell output

Cursor cannot replace shell output after a command has run, so the hook
rewrites the command before it runs. For `cargo build`:

```sh
{
cargo build
} > '/tmp/kompressor-cursor-<id>.out' 2>&1; '/usr/bin/kompressor' hook emit --config '…' --status "$?" '/tmp/kompressor-cursor-<id>.out'
```

The command runs in the same shell as before, so `cd` and `export` carry over;
its exit status is passed through; and if anything goes wrong, `emit` prints
the output unchanged. The model's transcript keeps the command it wrote. The
rewritten text shows up in one place: the header of a long command's terminal
file, which Cursor moves to the background after 30 seconds.

It is **off by default**. Cursor judges approval on the *rewritten* command,
literally, word by word. Measured on CLI 2026.11.22 without `--force`:

| Allow-list | Command | Rewritten, it |
|---|---|---|
| `Shell(seq)`, `Shell(kompressor)` | `seq 1 20000` | ran, compressed |
| `Shell(seq)`, `Shell(kompressor)` | `uname -a`, `seq 1 3 && uname -a` | was refused, as without the rewrite |
| `Shell(seq)` | `seq 1 20000` | was refused: `kompressor` itself is not approved |

So the rewrite never approves anything you had not, but it needs `kompressor`
approved, or every rewritten command is refused. The entry has to match the
binary as the rewrite names it: `Shell(kompressor)` matches the bare name,
used when `PATH` finds this binary; `Shell(/path/to/kompressor)` matches the
unquoted absolute path, used otherwise. Neither matches a quoted path. With
`only_when_approved = true`, the default, the hook rewrites only when the
CLI's settings (`~/.cursor/cli-config.json`, the workspace's
`.cursor/cli.json`) would run the result. The editor's approval settings are
not readable from outside, so there the rewrite needs
`only_when_approved = false`. A refused rewrite shows the model the rewritten
command in the refusal.

Left alone, always: commands that run `kompressor`; background jobs (`&`,
`nohup`); anything that wants a terminal (`vim`, `less`, `ssh host`, a REPL,
`git rebase -i`); output already redirected to a file; `exit`, `exec`, `set`,
`trap`; anything the tokenizer cannot balance; commands on `skip`; Windows.

## Verify

```bash
kompressor hook status cursor
```

says what is installed and whether each entry is still as written, what other
hooks answer the same events (the last reply wins), whether the store opens,
and **when a hook last ran**. If that says *never* after a session, Cursor is
not calling the hooks — a team policy can disable user hooks; `--print` the
entries for `/etc/cursor/hooks.json` instead.

```bash
kompressor stats by-tool
```

lists results per tool: seen, compressed, tokens removed, what shadow mode
says would have been removed, results *kept inline*, and MCP results Cursor had
already spilled to a file.

Tokens are counted only against what the model would have received without
compression. Cursor writes any tool result over 40 KB to a file and gives the
model a reference instead, so for such a result no tokens are claimed — the
row keeps its real size in `chars_in`, and it is counted under *kept inline*:
the model read it directly, compressed, instead of opening a file. What that
saves is the follow-up reads, which a hook cannot see. The dashboard has the same on its **Cursor** page, with the token usage
the desktop reports for each turn.

## When it does not work

| Symptom | Cause |
|---|---|
| `install` refuses: *already installed in …* | Cursor reads the user's and the workspace's hooks files both; two installs would run every hook twice and count every result twice. Uninstall one |
| A shell rewriter from another tool (RTK, lean-ctx) | Only one rewrite of a command survives — measured: a workspace hook's reply beat a user-level one. `install` refuses beside one in any hooks file Cursor reads; `--force` installs anyway, and then shell compression depends on which file wins |
| `hook status` says *last call: never* | Cursor is not running the hooks: a managed account, or (reported on macOS CLI 2026.11.05) the CLI not loading `~/.cursor/hooks.json`. Try `--project`. |
| MCP results never get smaller | They are under `min_compress_chars`, over Cursor's ~40 KB spill threshold (`stats by-tool` counts them as *spilled*), or from a tool on `skip_tools` |
| Shell output never gets smaller | `[hooks.cursor.shell] enabled` is false, or the CLI would refuse the rewrite: add `kompressor` to its allow-list (`--allow-list`). `hook status cursor` says which |
| Every shell command is refused | `only_when_approved = false` under an allow-list without a `kompressor` entry: add it, or set the option back to true |
| The model cannot retrieve | The `optscale_kompressor` server is disabled or unapproved; it can run `kompressor retrieve`, which the session-start context names |

Every failure in the hook changes nothing: it answers `{}`, and Cursor ignores
a hook that errors or times out.

## Limits

- **The saving is not measured yet.** Cursor already keeps any single tool
  result over ~40 KB out of the conversation, so only results under that, and
  the follow-up reads a smaller result avoids, can be won. Shadow mode
  (`--shadow`) is how to find out on your own work, before anything changes.
- No dollar figure is reported: a removed token is paid for once per later
  turn at a price Cursor sets, and a hook sees neither.
- Built-in Read, Grep, List and Fetch output cannot be compressed by any hook.
- No keep-warm, cache TTL or prefix handling: those act on provider requests
  that never pass through here.
- Cloud agents, Tab and Bugbot run on Cursor's machines.
- Shell rewriting is POSIX `sh`; it is off on Windows.
- `refs.db` now also holds tool output from Cursor sessions, for `refs.ttl`.
