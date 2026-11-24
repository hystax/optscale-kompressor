# Wrapping opencode

```bash
kompressor wrap opencode                     # the TUI, compressed
kompressor wrap opencode -- run "fix the failing test"
kompressor wrap opencode --attach            # OpenCode Desktop, and TUIs already running
```

Everything below was measured against **opencode 2.2.4** (CLI) and **OpenCode
Desktop 2.0.11** (whose bundled CLI is also 2.0.11) on Linux, against a
recording listener and against the live providers. opencode 2.x is a different
program from 1.x in the two ways that matter here, so none of this applies to a
1.x install.

## Contents

- [Why a variable is not enough](#why-a-variable-is-not-enough)
- [Launched sessions](#launched-sessions)
- [Desktop and the background service](#desktop-and-the-background-service)
- [Providers](#providers)
- [Permanent setup](#permanent-setup)
- [Measuring it](#measuring-it)
- [Known limits](#known-limits)
- [Troubleshooting](#troubleshooting)

## Why a variable is not enough

Earlier catalogues wrapped opencode by setting `OPENAI_BASE_URL`. On 2.x that
does worse than nothing, for two separate reasons.

**1. The variable breaks the provider.** With `OPENAI_BASE_URL` (or
`ANTHROPIC_BASE_URL`) in its environment, opencode 2.2.4 reports every model of
that provider as `Model unavailable` and sends no request at all. `wrap
opencode` sets neither, and the sweep removes both from the environment it
runs opencode in.

**2. The process that calls the model is not the one you start.** `opencode`
is a client of a long-lived background service, `opencode serve --service`,
recorded in `~/.local/state/opencode/service.json`. The service makes every
provider request with the environment *it* was started with, so a variable set
on the TUI reaches nothing. OpenCode Desktop uses the same service: it adopts
one that is running, or starts it with its own bundled CLI.

What does reach every provider is configuration:

```json
{ "providers": { "openai": { "settings": { "baseURL": "http://127.0.0.1:8817/v1" } } } }
```

`providers.<id>.settings.baseURL` is the 2.x spelling. The 1.x spelling,
`provider.<id>.options.baseURL`, is still honoured, and when one file holds
both, the 2.x one wins. `config.json` is no longer read at all; `opencode.json`
and `opencode.jsonc` are, and `.jsonc` is read second and wins.

## Launched sessions

`kompressor wrap opencode` adds two things to the command line it runs:

- **`OPENCODE_CONFIG_CONTENT`** holding the overlay above for every provider it
  routes. It outranks every config file, so nothing on disk can shadow it, and
  nothing on disk is written. If your shell already sets the variable, it is
  extended, not replaced.
- **`--standalone`**, so the session runs a private server that inherits the
  overlay instead of handing the turn to the background service, which would
  not see it. `--server URL` is refused for the same reason: that server never
  sees the overlay either.

```console
$ kompressor wrap opencode -- run "say hi"
kompressor: opencode providers through 127.0.0.1:8817: openai, anthropic, opencode, opencode-go
kompressor: proxy on 127.0.0.1:8817 (mode recoverable), log …
```

`--print` prints the variable, shell-quoted, for `eval`, and reminds you to
pass `--standalone` yourself.

## Desktop and the background service

```bash
kompressor wrap opencode --attach
```

This writes the same overlay into the **global** `opencode.json`
(`$XDG_CONFIG_HOME/opencode/`, or `opencode.jsonc` when that exists, because it
wins), then runs `opencode reload` if a background service is running.
Measured: the running service sent its next request to the new URL with no
restart, so a Desktop window or TUI that is already open is covered from that
moment. With no service running nothing is reloaded — `opencode reload` would
start one — and the next service to start reads the file. Ctrl-C (or SIGTERM)
puts the file back and reloads again on the same terms.

This reaches Desktop only on **Desktop 2.2.4 or newer**; see
[OpenCode Desktop needs 2.2.4 or newer](#opencode-desktop-needs-224-or-newer).

The edit follows the same rules as the Claude Code and Codex ones:

- The file is restored **byte for byte** when nothing else changed in it. Edits
  you make during the session survive, and a value of ours that you repointed
  yourself is left as you set it.
- Two attached sessions share the file; the last one out restores what was
  there before the first arrived.
- A session killed with SIGKILL leaves the file pointing at a dead proxy.
  `kompressor unwrap opencode` restores it and reloads the service.
- A file with comments or trailing commas is **refused**, not rewritten:
  rewriting it would lose them. Plain `kompressor wrap opencode` writes no file
  and has no such limit.

## Providers

| opencode provider | Base URL written                     | Proxy mount                | Upstream              |
| ----------------- | ------------------------------------ | -------------------------- | --------------------- |
| `openai`          | `http://127.0.0.1:PORT/v1`           | `/v1` (`openai`)           | api.openai.com        |
| `anthropic`       | `http://127.0.0.1:PORT/v1`           | `/v1/messages` (`anthropic`) | api.anthropic.com   |
| `opencode` (Zen)  | `http://127.0.0.1:PORT/zen/v1`       | `/zen` (`opencode-zen`)    | opencode.ai/zen/v1    |
| `opencode-go`     | `http://127.0.0.1:PORT/zen/go/v1`    | `/zen` (`opencode-zen`)    | opencode.ai/zen/go/v1 |

One `/zen` mount serves both tiers and every wire shape behind them. The wire
family is read off the path's tail, so Chat Completions, Responses and Messages
requests to Zen are each walked by the right parser. opencode's
`x-opencode-session` header is forwarded with the rest, which matters: the
relay answers `400 MissingSessionID` without it.

**`openai` is pinned to HTTP.** Left alone, opencode opens a WebSocket for the
main turn of an OpenAI model. The proxy relays and compresses it, but a relayed
turn reports no usage and runs no retrieve loop. The overlay sets
`providers.openai.settings.transport = "http"`, after which the same turn
reported its prompt tokens. A transport you set yourself is kept.

**A provider you routed is yours.** A `baseURL` already set for a provider, in
any config file opencode would read for that directory or in
`OPENCODE_CONFIG_CONTENT`, is left alone, and `wrap` says so:

```console
kompressor: opencode provider anthropic left on its own baseURL (https://gw.example/v1); it is not compressed
```

Overriding it would send your gateway's key to the vendor's host. Providers not
in the table (Bedrock, OpenRouter, a custom `@ai-sdk/openai-compatible` entry)
are not routed; add a `[[proxy.upstreams]]` mount for them, point their own
`baseURL` at it, and run `kompressor serve`.

**Upstreams.** A `wrap` that starts its own proxy mounts all three. A `wrap`
that adopts one already running — the packaged service, say — uses that
proxy's mounts, so it needs the `opencode-zen` mount the packaged configs now
carry. A config written before it routes `openai` and `anthropic` and 404s
Zen and Go.

## Permanent setup

`wrap opencode` is per session: `--attach` edits the config, holds a proxy up
and puts the file back on exit. To leave compression on for every opencode
session — Desktop, the TUI, `opencode run` — run kompressor as a service and
write the opencode side once. Everything served by the background service then
goes through the proxy, with no terminal left open.

### 1. kompressor, with the Zen mount

Any of the packaged setups works: the `.deb`/`.rpm` (`/etc/kompressor/config.toml`),
the macOS launch agent, or the systemd user service from
`scripts/service/install.sh` (`~/.config/kompressor/config.toml`). Configs from
this release carry every mount opencode needs. One written by an earlier
release has `openai` (`/v1`) and `anthropic` (`/v1/messages`) but no route for
OpenCode Zen or Go, which then 404. Check, and add it if it is missing:

```bash
grep -n 'path_prefix = "/zen"' /etc/kompressor/config.toml   # or your config path
```

```toml
[[proxy.upstreams]]
name = "opencode-zen"
path_prefix = "/zen"
base_url = "https://opencode.ai"
api_family = "auto"
forward_auth = true
```

If you only want some providers, the mounts each one needs are in the
[table above](#providers). Then restart the service (`sudo systemctl restart
kompressor`, `systemctl --user restart kompressor`, or reload the launch agent),
and note the port its `[server] listen` names — `8817` below; use yours.

```bash
curl -s http://127.0.0.1:8817/healthz && echo
```

### 2. opencode — one block in the global config

Add a `providers` block to `~/.config/opencode/opencode.json` (or
`opencode.jsonc` if you have one: it is read second and wins). Keep whatever
else the file holds:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "providers": {
    "openai":      { "settings": { "baseURL": "http://127.0.0.1:8817/v1", "transport": "http" } },
    "anthropic":   { "settings": { "baseURL": "http://127.0.0.1:8817/v1" } },
    "opencode":    { "settings": { "baseURL": "http://127.0.0.1:8817/zen/v1" } },
    "opencode-go": { "settings": { "baseURL": "http://127.0.0.1:8817/zen/go/v1" } }
  }
}
```

- Leave out a provider you do not use, and leave out one you route to a gateway
  of your own: a `baseURL` here replaces it, and your gateway key would then go
  to the vendor's host.
- `"transport": "http"` is what makes OpenAI turns measurable and recoverable;
  see [Providers](#providers). Leave it out to keep the WebSocket.
- `providers.<id>.settings` is the 2.x spelling. If the file already has a
  1.x `provider.<id>.options.baseURL`, this one wins; remove the old one to
  avoid confusion.

### OpenCode Desktop needs 2.2.4 or newer

Desktop runs the background service with **its own bundled CLI**, not the one
on your PATH, and replaces a service of any other version with it. Desktop
**2.0.11** bundles CLI 2.0.11, which does not reliably honour
`providers.<id>.settings.baseURL`: the setting parses and `opencode debug
config` shows it, but requests go straight to opencode.ai. Measured against a
recording listener, on Linux:

| Setup (CLI 2.0.11)                                         | Reached the listener |
| ---------------------------------------------------------- | -------------------- |
| `opencode-go` block only, no stored login                   | 3                    |
| same, `opencode-go` login stored                            | 0                    |
| same, OpenCode account login (`opencode` integration)       | 0, even with `apiKey` set |
| `opencode-go` and `opencode` blocks, no login               | 0                    |

`OPENCODE_CONFIG_CONTENT` and the 1.x `provider.<id>.options` spelling were
ignored the same way, and with the listener answering errors 2.0.11 fell back
to opencode.ai. There is no configuration that routes it dependably, so the
fix is the version: **CLI 2.2.4 honours the setting with every login above**
(8 requests reached the listener with the OpenCode account login stored).
Upgrade Desktop to 2.2.4 or newer — its updater, or the package from
opencode.ai — and check the version of the service it runs:

```bash
python3 -c "import json;print(json.load(open('$HOME/.local/state/opencode/service.json'))['version'])"
```

This applies to `kompressor wrap opencode --attach` too, which reaches Desktop
through the same service.

### 3. Apply it

```bash
opencode reload
```

The running service re-reads the file; an open Desktop window is covered from
its next request. If no service is running, nothing needs doing — the next one
to start (Desktop, or a TUI) reads the file. Quitting and reopening Desktop
also works.

### Verify it is working

Send a message from Desktop, then ask the service's statistics what arrived:

```bash
kompressor stats summary --stats /var/lib/kompressor/stats.db   # the [stats] path of your config
```

A request count of zero means opencode is not using the proxy — see
[Troubleshooting](#troubleshooting). `kompressor stats by-provider` shows which
mount each request took (`opencode-zen`, `openai`, `anthropic`).

### Turning it off

Remove the `providers` entries you added and run `opencode reload`. If the
service is stopped while opencode still points at it, every turn fails with a
connection error rather than falling back to the provider — stop the service
only after removing the block.

### Permanent setup and `wrap`

They can coexist. `kompressor wrap opencode` (launched sessions) ignores a
`baseURL` pointing at a kompressor proxy on 127.0.0.1 and routes the session
through its own overlay; a permanent block pointing elsewhere — another host,
another path — is treated as yours and left alone. `--attach` on top of a
permanent setup is not needed and only repoints the file for its own session.

## Measuring it

`scripts/kompressor-sweep/opencode.sh` is the opencode third of the live sweep,
beside `codex.sh` and `claude.sh`, with the same stages and verdicts:

```bash
scripts/kompressor-sweep/opencode.sh all
OPENCODE_SWEEP_MODEL=opencode-go/glm-5.3 scripts/kompressor-sweep/opencode.sh sweep
```

The default model is `anthropic/claude-haiku-5-5`, billed to the Anthropic API
key in your opencode login. Before any paid stage the script sends the model
one word with the proxy out of the path, and stops — with the provider's own
error and what to do about it — if that fails, so an expired or exhausted key
costs one short request rather than three failed stages. `SKIP_PREFLIGHT=1`
skips the check.

Every session goes through `kompressor wrap opencode`, so the stages test the
wrap as well as the engine. opencode runs from a home of the sweep's own,
`$WORK/opencode-home`: your MCP servers, plugins and instructions stay out of
the measured prompt, its sessions stay out of your history, and the
background service your TUI and Desktop share is never touched. Only the
login is borrowed — opencode 2.x keeps it in the `credential` table of
`opencode.db`, not in `auth.json`, so the sweep copies those rows (and nothing
else) on every run.

opencode cuts a tool result at 2,000 lines or 51,200 bytes and sends the model
the head; the sweep's own config raises `tool_output` so the proxy sees each
fixture whole, and a run where opencode still cut one is VOID.

## Known limits

- **Retrieval tools as MCP.** `wrap claude` hands Claude Code the retrieve tool
  as an MCP server for the session. opencode gets the in-band tool the proxy
  injects into every request, which covers HTTP turns; a WebSocket turn (an
  `openai` model with `transport` set back to `websocket`) has none.
- **The `--attach` overlay is global.** Every opencode session served by the
  background service goes through the proxy while it is attached, not only the
  one you meant.
- **Desktop's bundled CLI** is a different version from the one on your PATH,
  and Desktop 2.0.11's CLI does not reliably honour a provider's `baseURL` — see
  [OpenCode Desktop needs 2.2.4 or newer](#opencode-desktop-needs-224-or-newer).
- **ChatGPT-plan `openai`** (opencode's OAuth login for OpenAI) was not
  measured. The `/v1` mount follows a ChatGPT credential to chatgpt.com, as it
  does for Codex.

## Troubleshooting

**`Model unavailable: openai/…`** — `OPENAI_BASE_URL` or `ANTHROPIC_BASE_URL`
is set in the environment opencode runs in. Unset it.

**Nothing reached the proxy** (`wrap` ends with `0 requests`) — the session
was served by the background service. Use the plain wrap, which runs
`--standalone`, or `--attach`, which reloads the service.

**`401 … api_key_expired`** — the key itself expired at the provider, and
signing in again with it does not renew it: create a new key (OpenCode Console
for `opencode` and `opencode-go`) and store it with `opencode auth login` or
`/connect`. **`Go requires a credential`** — no login is stored for that
provider. Either way the proxy forwarded the provider's answer as it should.

**`Free usage limit reached`** — the free Zen models are exhausted for this
login; pick a model on a provider you pay for.

**Desktop still talks to the provider directly** — work through these in order:

1. `python3 -c "import json;print(json.load(open('$HOME/.local/state/opencode/service.json'))['version'])"`
   names the CLI serving Desktop. Below 2.2.4, upgrade Desktop
   ([why](#opencode-desktop-needs-224-or-newer)). An upgrade installed while the
   old service runs leaves it serving: quit Desktop and run
   `opencode service restart`.
2. `opencode debug config` must list your `opencode.json` as a `document`. If it
   does not, the file failed to parse — a missing brace is enough — and opencode
   ignores the **whole file**, MCP servers included, without saying so. Check it
   with `python3 -m json.tool ~/.config/opencode/opencode.json`.
3. Run `opencode reload`. A service started with an `OPENCODE_CONFIG_CONTENT` of
   its own keeps its own view.

**`opencode reload` fails with `DirectoryNotFoundError`** — the service still
holds a project directory that was deleted since it opened it, and `reload`
re-resolves every one. `opencode service restart` clears it.
