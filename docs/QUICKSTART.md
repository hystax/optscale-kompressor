# QUICKSTART

Every way to run OptScale Kompressor, shortest first. All output below is real — copied
from a run, not written from memory.

```bash
make release
./target/release/kompressor version
```

(`cargo build --release -p kompressor-cli` does the same thing; `make` on its own
lists the rest.)

Everything after this assumes `kompressor` is on your `PATH`.

---

## 1. Wrap an agent

The short path. Nothing to configure, nothing to clean up.

```bash
kompressor wrap claude
```

That picks a port, starts the proxy with an Anthropic upstream mounted, waits
for it to answer, sets `ANTHROPIC_BASE_URL` for the agent, runs `claude` in the
foreground, and stops the proxy when you quit. Your own arguments go after `--`:

```bash
kompressor wrap claude -- --model opus
kompressor wrap aider -- --model sonnet
```

**What `wrap` does and does not promise.** For most agents it puts the base-URL
variable the agent documents into that agent's environment. Whether the agent
*reads* that variable for the provider you are using is the agent's business,
and the entries below differ:

- **codex** ignores `OPENAI_BASE_URL` entirely (measured on 0.155.1), so it is
  wrapped by editing `$CODEX_HOME/config.toml` instead — marker-delimited
  blocks added at startup and removed on exit, with everything outside them
  preserved byte for byte. What gets written depends on how codex is logged
  in, because the two modes need different blocks and each one's block breaks
  the other: an API-key install gets a custom provider, a ChatGPT plan gets a
  single `openai_base_url` override. `wrap` reads the mode from codex's own
  `auth.json`; `kompressor unwrap codex` removes a block left behind by a
  session that was killed. [CODEX.md](CODEX.md) has the details.
- **opencode** reads it only for its built-in `openai` provider. A provider
  declared in `opencode.json` carries its own `options.baseURL`, and config
  beats environment — so point that provider at the proxy instead.
- **claude** reads it and routes correctly, but pins its whole prompt with a
  `cache_control` breakpoint, so a wrapped session compresses nothing by design.
  [CLAUDE-CODE.md](CLAUDE-CODE.md) explains why that is the right outcome.

You do not have to remember any of this: `wrap` counts what reached the proxy
and tells you on the way out, which is how each of the above was found.

Which agents are known:

```console
$ kompressor wrap --list
Agents kompressor wrap knows:
  cursor-agent  Cursor Agent CLI — cannot be compressed (see below)
  cursor        Cursor (GUI editor; base URL set by hand in Settings)
  claude        Claude Code (Anthropic Messages)
  codex         OpenAI Codex CLI (OpenAI Responses), via its config file
  opencode      opencode (OpenAI Chat; only for its built-in `openai` provider)
  aider         aider (OpenAI Chat or Anthropic Messages, by model)

Anything else: kompressor wrap --env NAME=root|v1 -- <command> [args]
`root` for a base URL the client appends /v1/… to, `v1` for one that already ends in /v1.
```

### Anything not in that list

Name the variable your tool reads and the URL shape it wants. `root` is for a
client that appends `/v1/messages` itself; `v1` is for one that expects a URL
already ending in `/v1`:

```bash
kompressor wrap --env OPENAI_BASE_URL=v1 -- my-agent --flag
kompressor wrap --env LLM_BASE_URL=v1 --env ANTHROPIC_BASE_URL=root -- some-tool
```

To put the variables into your *current* shell instead of launching anything:

```console
$ kompressor wrap claude --print
export ANTHROPIC_BASE_URL=http://127.0.0.1:8817
```

```bash
eval "$(kompressor wrap claude --print)"   # needs a proxy already running
```

### Useful flags

| Flag | What it does |
|---|---|
| `--mode aggressive` | compress harder for this session; nothing is retained (§4) |
| `--port 9000` | pin the port instead of taking the configured one |
| `--config ./kompressor.toml` | run the proxy under a specific configuration |
| `--no-proxy` | require one to be running already; fail rather than start one |
| `--attach` | configure an agent this command does not launch (below) |

### Two terminals

Run `kompressor wrap` in as many terminals as you like. The first starts a
proxy; the rest find it and use it. Whoever leaves last turns it off:

```console
# terminal 1
$ kompressor wrap claude
kompressor: proxy on 127.0.0.1:8817 (mode recoverable), log ~/.kompressor/logs/proxy-8817.log

# terminal 2
$ kompressor wrap codex
kompressor: using the proxy already listening on 127.0.0.1:8817
```

Quitting terminal 2 leaves the proxy up for terminal 1. Quitting terminal 1 then
stops it. A session that is killed outright leaves a marker behind; the next
session sweeps it, because a marker is believed only while its owner is alive.

### Editor-hosted agents: `--attach`

Environment variables only reach a process this command starts. An agent
launched by your editor never sees them, so for those the base URL goes into the
settings file the agent reads:

```console
$ kompressor wrap claude --attach
kompressor: proxy on 127.0.0.1:8817 (mode recoverable), log ~/.kompressor/logs/proxy-8817.log
kompressor: /your/project/.claude/settings.local.json ANTHROPIC_BASE_URL=http://127.0.0.1:8817
kompressor: attached. Ctrl-C restores the settings and releases the proxy.
```

Ctrl-C puts the file back exactly as it was, including a base URL you had set
yourself. If the session is killed before it can, recover with:

```bash
kompressor unwrap claude
```

---

## 2. The proxy, for any client

`wrap` is a convenience over this. Point any OpenAI- or Anthropic-compatible
client at the listener:

```bash
kompressor serve --proxy --api --config kompressor.toml
export OPENAI_BASE_URL=http://127.0.0.1:8817/v1
```

The proxy needs to know where to forward. The longest matching path prefix wins,
so a catch-all mount cannot hide a more specific provider:

```toml
[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/v1/messages"
base_url = "https://api.anthropic.com"
forward_auth = true                   # the client's own key is passed through
api_family = "anthropic"              # optional; "passthrough" disables parsing

[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "https://api.openai.com"
forward_auth = true
api_family = "openai"
```

`api_family` defaults to `"auto"`: known endpoints are detected from the
forwarded path. Set `"passthrough"` explicitly for endpoints the proxy should
relay without parsing.

`path_prefix` and `strip_prefix` are flat keys, and unknown keys are refused —
a `match = { … }` table is a parse error, not a silently ignored mount.

Mount several providers under names of your own with `strip_prefix`:

```toml
[[proxy.upstreams]]
name = "local"
path_prefix = "/ollama/"
strip_prefix = true                   # /ollama/v1/chat/… reaches the upstream as /v1/chat/…
base_url = "http://127.0.0.1:11434"
```

In `recoverable` mode the proxy also offers the model a retrieve tool and
**executes it itself**, so a client that has never heard of OptScale Kompressor still
gets elided content back when the model asks for it.

---

## 3. The HTTP API

For a harness that wants to compress explicitly rather than in flight.

```bash
kompressor serve --api --config kompressor.toml
```

```console
$ curl -s localhost:8817/v1/compress -H 'content-type: application/json' -d '{
    "messages": [
      {"role": "user", "content": "what failed?"},
      {"role": "tool", "content": "<300 lines of log>"}
    ]}' | jq .stats
{
  "tokens_in": 8770,
  "tokens_out": 635,
  "ratio": 0.07240593,
  "chars_in": 18991,
  "chars_out": 1377,
  "compressed_messages": 1,
  "skipped_messages": 1,
  "by_type": { "log": 1 },
  "mode": "recoverable",
  "refs_emitted": 1,
  "ref_bytes_emitted": 17639,
  "router_version": "algorithmic-v2-rs"
}
```

The tool turn comes back with the repetition folded and a handle for what left:

```text
2026-09-18 10:00:00 INFO  [worker-0] processed job 0 in 0ms [×280 ref:k_CQGMaWIdTfth]
2026-09-18 10:00:40 INFO  [worker-0] processed job 280 in 280ms
…
```

The user turn is byte-identical: `skipped_messages: 1`.

Ask for the fragment back:

```console
$ curl -s localhost:8817/v1/retrieve -H 'content-type: application/json' \
      -d '{"ref":"k_CQGMaWIdTfth"}' | jq '{found, total_lines, total_chars}'
{ "found": true, "total_lines": 279, "total_chars": 17639 }
```

| Endpoint | Purpose |
|---|---|
| `POST /v1/compress` | a conversation in, a conversation out |
| `POST /v1/compress/raw` | one string in, one string out |
| `POST /v1/retrieve` | a handle in, the elided bytes out |
| `POST /v1/detect` | classify a payload without compressing it |
| `GET /v1/stats/summary` `by_type` `histogram` `requests` `retrievals` `store` | the same reports as `kompressor stats` — mounted only when `stats.enabled` (the default); with statistics off the whole prefix 404s |
| `GET /healthz` `/readyz` `/metrics` | liveness, readiness, Prometheus |

`/v2` is served as an alias of `/v1` so a client pinned to either keeps working.

---

## 4. MCP

Gives an agent runtime the retrieve tool directly, for setups where the proxy is
not in the path.

```bash
kompressor serve --mcp --config kompressor.toml
```

```console
$ curl -s localhost:8817/mcp -H 'content-type: application/json' \
      -H 'accept: application/json, text/event-stream' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq -r '.result.tools[].name'
optscale_kompressor_compress
optscale_kompressor_retrieve
…
```

Streamable HTTP, stateless — each POST is independent, so it scales
horizontally without sticky sessions.

---

## 5. The CLI, on a file

No server involved.

```console
$ kompressor detect big.log
content_type  build
confidence    0.85
strategy      log

$ kompressor compress big.log --config kompressor.toml | head -2
2026-09-18 10:00:00 INFO  [worker-0] processed job 0 in 0ms [×280 ref:k_CQGMaWIdTfth]
2026-09-18 10:00:40 INFO  [worker-0] processed job 280 in 280ms
```

18,980 characters to 1,366.

Recoverable CLI compression retains fragments in the configured SQLite
`refs.path`, under the anonymous tenant, before printing their handles. The
default path is `data/refs.db`. `retrieve` reads the same configuration
`compress` wrote under — the same `--config` file, or the same discovered one —
so a handle resolves without naming a store at all; `--refs` overrides that
path when you want to read a store the configuration does not name. A memory
backend or a failed retention check returns the original payload, with no new
handles, because fragments must survive the compression process exiting.

When a payload does *not* compress and you want to know why, `--explain` reports
every decision — the detected type, which reading won, and whether the
accounting guard refused the result:

```bash
kompressor compress mystery.txt --explain
```

Fetch a fragment back without a server, narrowing it so you pay for what you
need rather than the whole thing:

```console
$ kompressor retrieve k_CQGMaWIdTfth --refs data/refs.sqlite --lines 1:2
2026-09-18 10:00:01 INFO  [worker-1] processed job 1 in 1ms
2026-09-18 10:00:02 INFO  [worker-2] processed job 2 in 2ms
```

The proxy keeps its injected retrieval tool private in both buffered and
streamed mixed-tool responses, while preserving client tool calls. A model
that keeps requesting retrieval after two budget refusals receives a 502 error
in buffered mode or a provider-format SSE error in streaming mode. Failed
hidden rounds also end as errors, without a successful completion marker.

`--pointer` takes an RFC 6901 pointer into a JSON fragment; `--grep` returns
matching lines with two lines of context.

---

## 6. Modes

| Mode | What it promises | Use it when |
|---|---|---|
| `recoverable` **(default)** | Elides, and keeps every removed fragment behind a handle | Something can retrieve — the proxy, or an agent with the MCP tool |
| `aggressive` | Elides, retains nothing; every elision still carries a marker | Nothing can retrieve, and you would rather have the tokens |
| `lossless` | Only reversible transforms; nothing is dropped | The payload has to survive intact |

```bash
kompressor wrap claude --mode aggressive
kompressor compress big.log --mode lossless
```

Recoverable is only useful when something can fetch a handle back. Through the
proxy that is automatic. Through the bare API with no retriever, prefer
`aggressive` — the handles would be minted and never used.

---

## 7. Reading the savings honestly

Gross savings are not the number that matters. Here is the same session as
above, after retrieving the fragment back once:

```console
$ kompressor stats summary --stats data/stats.sqlite
tenant
requests            1
messages            2 total, 1 compressed
tokens              8770 in, 635 out
saved gross         8135
retrieved back      8145 over 1 retrievals (1 hits, 100.0% hit rate)
saved net           -10
```

**Net is negative.** Compression removed 8,135 tokens and the retrieval put
8,145 back — the whole fragment, plus framing. That is the failure mode
`recoverable` has and `aggressive` does not, and it is why `saved_net` is
reported next to `saved_gross` rather than left for someone to work out.

Two things keep net positive in practice: the model usually asks for a fraction
of what was elided, and ranged retrieval (`--lines`, `--pointer`, `--grep`) puts
back only the part it asked about. An unnarrowed retrieve loop can cost more
than the compression saved.

Also: a windowed sum of gross over-counts on agent traffic, because every turn
re-compresses the same growing history and the same bytes are credited each
time. Measured per-turn savings on real workloads have come out roughly an order
of magnitude below the windowed headline.

```bash
kompressor stats by-type    --stats data/stats.sqlite   # which compressors are earning
kompressor stats retrievals --stats data/stats.sqlite   # hits, expiries, tokens put back
kompressor stats request ID --stats data/stats.sqlite   # one request, end to end
kompressor stats report     --stats data/stats.sqlite > report.html   # all of it, one page
```

---

## 8. Configuration

A file is found at `$KOMPRESSOR_CONFIG`, then `./kompressor.toml`, then
`$XDG_CONFIG_HOME/kompressor/config.toml`. Any key can also be set as
`KOMPRESSOR__SECTION__KEY` (**two** underscores), which wins over the file.

```bash
kompressor check-config --config kompressor.toml   # the effective result, secrets redacted
```

A working starting point:

```toml
[engine]
mode = "recoverable"
min_compress_chars = 512        # payloads under this are left alone
ref_ttl_seconds = 3600          # idle window, restarted on every read

[refs]
backend = "sqlite"              # sqlite | memory
path = "data/refs.sqlite"

[stats]
path = "data/stats.sqlite"

[proxy]
min_tokens = 512                # skip the compression hop below this
```

Per-session overrides without touching the file:

```bash
KOMPRESSOR__ENGINE__MODE=aggressive kompressor wrap claude
```

**Paths are never expanded.** `$HOME` and `~` in a TOML path are taken
literally — a `path = "$HOME/.local/share/kompressor/refs.db"` creates a
directory *called* `$HOME` under whatever the working directory happens to be.
Write absolute paths, or let a service template substitute them at install time
(below).

---

## 9. Running it as a service

Two shapes, for two different jobs.

**A user service**, for a proxy the agents on your own login talk to. It runs as
you, reads and writes only your files, and needs no root:

```bash
scripts/service/install.sh            # write config + unit, enable, start
systemctl --user status kompressor
scripts/service/uninstall.sh          # --purge also removes config and state
```

The installer resolves `$HOME` itself and writes absolute paths into
`$XDG_CONFIG_HOME/kompressor/config.toml`, validates the result with
`check-config` *before* enabling anything — a unit that starts and then dies on
a bad config is a worse outcome than a failure while the message is still on
your terminal — and refuses to overwrite an existing config without `--force`.

A user service stops when your last session ends. `loginctl enable-linger $USER`
keeps it running while you are logged out.

**A system service**, for a machine-wide daemon. `make deb` and `make rpm` build packages
that installs the binary, `/etc/kompressor/config.toml` (as a conffile, so an
upgrade will not overwrite your edits), and a unit that runs as an unprivileged
`kompressor` user with its state in `/var/lib/kompressor`.

**A static package**, for a machine whose libc you do not control.
`make deb-static` and `make rpm-static` build the same two packages from a
statically linked musl binary. They install the same files and run the same
unit; the difference is that neither depends on a libc, so both install on a
distribution older than the one you built them on.

The ordinary `make deb` and `make rpm` record the *build host's* glibc as a
minimum — build on a current Ubuntu and the packages refuse to install on
anything a few years older, which is only ever discovered on the far machine.
Measured on this build: the glibc `.deb` demands `libc6 (>= 2.39)` and will not
install on Debian 11, and the glibc `.rpm` demands seven `GLIBC_2.29`-and-later
symbol versions and will not install on Rocky Linux 8. Both static packages
install and run on those same two images.

They need the Rust musl target and a musl C compiler (the tree-sitter grammars
are C):

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt install musl-tools
make deb-static        # target/debian-static/kompressor_<version>-static_<arch>.deb
make rpm-static        # target/generate-rpm-static/kompressor-<version>-static.<arch>.rpm
```

The filename is the only place the difference shows. The **package** name and
version are the same as the glibc build's, deliberately, so `dpkg`/`rpm` treat
the two as one program and moving between them works in either direction. To
check which one you have:

```bash
dpkg-deb -f <file> Depends          # static: just `adduser`
rpm -qp --requires <file>           # static: no libc.so.6 lines
```

The config is installed **world-readable** (`0644 root:root`). Nothing in the
shipped file is a credential — every upstream sets `forward_auth = true`, so the
caller's own key is forwarded and none is stored — and an operator has to be
able to read where traffic is being sent. **If you add a secret** (an `[auth]`
token, or an upstream key with `forward_auth = false`), tighten it at the same
time; the service account is in the `kompressor` group, so this keeps working:

```bash
sudo chown root:kompressor /etc/kompressor/config.toml
sudo chmod 0640 /etc/kompressor/config.toml
```

The state directory is world-readable too (`0755`, with `UMask=0022` giving
`0644` databases), so `kompressor stats` against the system deployment needs no
`sudo`. That is a deliberate trade and worth knowing: **`refs.db` holds the
content the engine elided** — source, logs, command output — so every local
account on the machine can read it. On a shared host, set `StateDirectoryMode=0750`
and `UMask=0027` in the unit and add the people who need it to the `kompressor`
group.

**On macOS**, `make macos-pkg-docker` builds a `.pkg`. [`MACOS.md`](MACOS.md)
is the step-by-step version of this with Codex wiring included. It cross-compiles in a
container, so the machine building it needs Docker and not a Mac — zig supplies
the macOS libc stubs, and `xar` and `mkbom` are open-source readers of the two
container formats a `.pkg` is made of. Nothing Apple-proprietary is involved.

```bash
sudo installer -pkg target/macos/kompressor-*-universal2.pkg -target /
```

Not a double-click: the package is **unsigned**, so Gatekeeper refuses one.
Right-click → Open works too. Signing needs an Apple Developer ID.

It installs a **LaunchAgent**, not a LaunchDaemon — the same decision as the
systemd *user* unit, for the same reason: the proxy serves the agents in a login
session and reads that user's config, which a root daemon has no business doing.
Two consequences worth knowing:

* it starts at **login**, not at boot, and does not run while you are logged
  out. There is no `enable-linger` equivalent;
* the config lands at `~/.config/kompressor/config.toml`, which **is** on the
  loader's search path — so unlike the Linux system service below, a bare
  `kompressor stats` finds the right store with no flags and no `sudo`.

State goes to `~/Library/Application Support/kompressor/`, logs to
`~/Library/Logs/kompressor.log`. `launchctl print gui/$(id -u)/com.kompressor.proxy`
says whether it is loaded; `launchctl bootout gui/$(id -u)/com.kompressor.proxy`
stops it until next login, and `launchctl disable` stops it for good.

### Upgrading

Installing a newer package over an older one. Both routes keep your
configuration and your data; the stores migrate on the first start afterwards.

**The `.deb`** — `sudo dpkg -i kompressor_*.deb`, or `sudo apt install ./kompressor_*.deb`:

* `/etc/kompressor/config.toml` is a conffile, so your edits are kept. dpkg only
  asks what to do when the *shipped* default changed as well, and when it does
  it leaves the other version beside it as `.dpkg-dist` or `.dpkg-old`;
* `/var/lib/kompressor` is not touched. `refs.db` carries over, so a handle a
  live conversation still holds keeps resolving across the upgrade, and
  `stats.db` keeps its history;
* the unit is **restarted**, not merely reloaded, so the new binary is the one
  serving when `dpkg` returns;
* the statistics schema migrates on that first start, one transaction per step.

**The `.pkg`** — `sudo installer -pkg kompressor-*-universal2.pkg -target /`:

* the package identifier is the same in every release (`com.kompressor.cli`), so
  Installer upgrades the component rather than leaving two copies;
* `~/.config/kompressor/config.toml` is written only when it does not already
  exist, so your edits survive;
* `~/Library/LaunchAgents/com.kompressor.proxy.plist` is **regenerated every
  time**. It is a template with your home directory substituted in, so changes
  made to it directly do not survive an upgrade — change the config, not the
  plist;
* `~/Library/Application Support/kompressor/` is left alone, and migrates on the
  first start like the Linux one;
* the agent is `bootout`ed and `bootstrap`ped again, so the new binary is
  running when the installer finishes rather than at the next login.

### Removing it

Four install routes, four removals. All of them leave the **fragment store**
alone unless you say otherwise — deleting it is not tidying up: a handle a
conversation still holds stops resolving, so an agent mid-task loses what was
elided from it.

**The systemd user service:**

```bash
scripts/service/uninstall.sh            # stop, disable, remove the unit
scripts/service/uninstall.sh --purge    # and the config and state as well
```

**The `.deb`.** `remove` keeps `/etc/kompressor` and `/var/lib/kompressor`;
`purge` removes both along with the service account:

```bash
sudo apt remove kompressor      # or: sudo apt purge kompressor
```

**The `.rpm`.** RPM has no purge/remove distinction, so an ordinary `remove`
is the only removal there is — which is why it deliberately keeps the state and
tells you the command instead of running it:

```bash
sudo dnf remove kompressor
sudo rm -rf /var/lib/kompressor && sudo userdel kompressor && sudo groupdel kompressor
```

**The macOS `.pkg`.** macOS has no uninstall verb, so this is by hand. Unload
the agent first, then the files, then the receipt:

```bash
launchctl bootout gui/$(id -u)/com.kompressor.proxy
rm ~/Library/LaunchAgents/com.kompressor.proxy.plist
sudo rm -rf /usr/local/bin/kompressor /usr/local/share/kompressor /usr/local/share/doc/kompressor
sudo pkgutil --forget com.kompressor.cli
```

`pkgutil --forget` only drops the receipt — it never deletes a file, which is
why the `rm`s come first. `pkgutil --files com.kompressor.cli` lists what the
package placed, if you would rather check before removing. Your config, state
and logs are left: `~/.config/kompressor`, `~/Library/Application Support/kompressor`
and `~/Library/Logs/kompressor.log`.

### Reading the stats of a system service

`/etc` is **not** on the configuration search path. The loader looks in exactly
three places, first match wins:

| | |
|---|---|
| 1 | `$KOMPRESSOR_CONFIG` |
| 2 | `./kompressor.toml` — the working directory |
| 3 | `$XDG_CONFIG_HOME/kompressor/config.toml`, or `~/.config/kompressor/config.toml` when that variable is unset |

`/etc/kompressor/config.toml` appears nowhere in that list: the service reads it
only because the unit passes `--config` explicitly. So a bare `kompressor stats`
run by a human does not find the system config. Worse, if you also have a user
config it finds *that* one and answers from the wrong store without saying so:

```bash
$ kompressor stats summary      # with a user config present
requests   6                    # your own store, not /var/lib/kompressor
```

Three ways to ask the right one:

```bash
# the search path's first entry, so no flag is needed
KOMPRESSOR_CONFIG=/etc/kompressor/config.toml kompressor stats summary

# or skip configuration entirely
kompressor stats --stats /var/lib/kompressor/stats.db summary

# or name it explicitly
kompressor stats --config /etc/kompressor/config.toml summary
```

Exporting `KOMPRESSOR_CONFIG` in a shell rc makes the first one the default —
at the cost of shadowing your *user* store for every invocation, which is the
same trap pointing the other way.

Two more properties of that order are worth knowing before they surprise you.
`$KOMPRESSOR_CONFIG` is used **even when the file does not exist**, and the
error names the path — deliberately, so a typo in an explicit setting is
reported rather than silently falling through to a different file and answering
from the wrong store. And entry 2 means the directory you are standing in can
decide the answer: `kompressor stats summary` run inside a checkout that happens
to contain a `kompressor.toml` reads a different database than the same command
run from your home directory. Whichever file wins, `KOMPRESSOR__*` environment
variables are layered over it, so a setting can differ from the file without the
file being wrong.

No `sudo` is needed for any of them. The packaged unit ships
`StateDirectoryMode=0755` and `UMask=0022`, so both databases land `0644` — the
deliberate trade described above, and the reason the commands here are plain
`kompressor` rather than `sudo kompressor`. It also means every local account
can read `refs.db`, which holds the content the engine elided. On a shared host,
tighten the unit to `StateDirectoryMode=0750` and `UMask=0027`, add the people
who need it to the `kompressor` group, and these commands then need that
membership rather than `sudo`.

---

## 10. When something is wrong

**The agent works but nothing is compressed.** It is talking to the provider
directly. Check the variable actually reached it — `kompressor wrap … --print`
shows what would be set — and that your tool reads that name rather than another.

**`the proxy did not answer /healthz`.** The log path is in the error. Most
often the port is taken by something else (`--port` picks another) or the
configuration failed to validate (`kompressor check-config`).

**Everything 404s through the proxy.** No upstream matched. `proxy.upstreams` is
empty by default, and `wrap` only mounts the providers of the agent it knows —
so a hand-rolled `--env` wrap needs its upstream in the config.

**Everything works but nothing is compressed.** Check whether the mount sets
`api_family = "passthrough"`, whether the endpoint is recognised, and whether
the client pinned the whole prompt. `kompressor check-config` prints the
effective family.

**An Anthropic request reached OpenAI.** Check the mount prefixes and the
forwarded path; the most specific matching path prefix wins.

**A settings file still points at a dead proxy.** `kompressor unwrap claude`.

**Where things live.** `$KOMPRESSOR_HOME` (default `~/.kompressor`) holds
`run/` — generated configs and the client markers that reference-count a shared
proxy — and `logs/proxy-<port>.log`. Fragments and statistics are separate, under
the `refs.path` and `stats.path` in your configuration.
