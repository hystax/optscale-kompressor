# OptScale Kompressor

![CI](https://github.com/hystax/optscale-kompressor/actions/workflows/ci.yml/badge.svg)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![MSRV 1.95](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

**Deterministic prompt compression for coding agents, in one Rust binary.**

OptScale Kompressor sits between a coding agent and its model provider. It
removes the repetitive tool output agents re-send on every turn, and on Claude
Code it keeps the provider's prompt cache alive while tools run.

Install it (Linux and macOS; [other platforms](#install)):

```bash
curl -fsSL https://raw.githubusercontent.com/hystax/optscale-kompressor/main/install.sh | sh
```

Then run your agent through it:

```bash
kompressor wrap codex
```

That starts the proxy, points Codex at it, runs Codex, and cleans up on exit.
`wrap` also knows `claude`, `opencode` and `aider`. When the session ends it
tells you what happened:

```console
kompressor: 26 request(s), 126 message(s) compressed, ~2029706 tokens saved (2015180 net of retrieval)
```

Measured on Terminal-Bench 2.1, as the difference between real provider bills:

| Agent | Bill vs no compression | Tasks solved | What does it |
| --- | --- | --- | --- |
| Codex on GPT-5.5 | **−22.7%**, about twice Headroom's savings | 81.1% vs 82.2% | [Cache-safe compression of tool output](#feature-cache-safe-compression) |
| Claude Code on Opus 5.5 | **−20%**, with 99% fewer tokens lost to cache expiry | 90.6% vs 89.9% | [Keeping the prompt cache alive](#feature-claude-code-cache) |

[Results](#results) · [Reading the savings honestly](#reading-the-savings-honestly) ·
[Not built yet](#not-built-yet)

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/hystax/optscale-kompressor/main/install.sh | sh
```

No Rust toolchain needed. [`install.sh`](install.sh) downloads the latest
release, checks it against the release's `SHA256SUMS`, and stops on a mismatch:

- **Linux x86_64 and arm64:** the binary goes to `~/.local/bin`, and the
  packaged configuration to `~/.config/kompressor/config.toml` unless one is
  already there. No root. The x86_64 binary is static; the arm64 one needs
  glibc 2.28 or newer.
- **macOS (Apple silicon and Intel):** the universal `.pkg` is installed with
  `sudo installer`: the binary in `/usr/local/bin`, a user config, and a
  LaunchAgent that serves the proxy. Details: [docs/MACOS.md](docs/MACOS.md).

`KOMPRESSOR_VERSION=2.6.0` pins a release and `KOMPRESSOR_INSTALL_DIR` moves the
Linux binary. Then check it runs:

```bash
kompressor version
```

<a id="quick-start-install"></a>
### Packages

To install a system package instead, or on another platform, download it from
the [latest release](https://github.com/hystax/optscale-kompressor/releases/latest)
and check it against `SHA256SUMS` from the same page
(`sha256sum --check --ignore-missing SHA256SUMS`).

| Platform | File | Install |
| --- | --- | --- |
| Debian, Ubuntu | `kompressor_<version>-1_amd64.deb` | `sudo apt install ./kompressor_*-1_amd64.deb` |
| RHEL, Fedora | `kompressor-<version>-1.x86_64.rpm` | `sudo dnf install ./kompressor-*-1.x86_64.rpm` |
| Any Linux x86_64 | `kompressor-<version>-x86_64-linux-musl` (static) | `install -m 755 kompressor-*-x86_64-linux-musl ~/.local/bin/kompressor` |
| Debian, Ubuntu on arm64 | `kompressor_<version>-1_arm64.deb` | `sudo apt install ./kompressor_*-1_arm64.deb` |
| RHEL, Fedora on aarch64 | `kompressor-<version>-1.aarch64.rpm` | `sudo dnf install ./kompressor-*-1.aarch64.rpm` |
| Any Linux arm64, glibc 2.28+ | `kompressor-<version>-aarch64-linux-gnu` | `install -m 755 kompressor-*-aarch64-linux-gnu ~/.local/bin/kompressor` |
| macOS (Apple silicon and Intel) | `kompressor-<version>-universal2.pkg` | `sudo installer -pkg kompressor-*-universal2.pkg -target /` |
| Windows x86_64 | `kompressor-<version>-x86_64-setup.exe` | run it; see [docs/WINDOWS.md](docs/WINDOWS.md) |
| FreeBSD x86_64 | `kompressor-<version>-x86_64-freebsd.pkg` | `sudo pkg add ./kompressor-*-x86_64-freebsd.pkg` |

Older glibc systems can use the `-static` `.deb` / `.rpm` builds instead. With
the [GitHub CLI](https://cli.github.com) the download is one command, for
example:

```bash
gh release download --repo hystax/optscale-kompressor -p 'kompressor_*-1_amd64.deb' -p SHA256SUMS
```

[docs/QUICKSTART.md](docs/QUICKSTART.md) covers running it as a service, and
[docs/WINDOWS.md](docs/WINDOWS.md) the Windows paths, including Claude Desktop
and the Codex app.

### From source

With Cargo (Rust 1.95 or newer):

```bash
cargo install --locked --git https://github.com/hystax/optscale-kompressor kompressor-cli
```

Or from a checkout:

```bash
git clone https://github.com/hystax/optscale-kompressor
cd optscale-kompressor
make release
./target/release/kompressor version
```

`make deb`, `make rpm`, `make static` (musl), `make linux-arm64-all`,
`make docker`, `make macos-pkg-docker`, `make win` and `make win-installer`
build packages; each
says what it needs if a tool is missing.

---

## Quick start

### Codex

```bash
kompressor wrap codex
```

Codex ignores `OPENAI_BASE_URL` (measured on 0.155.1), so `wrap` adds a
marker-delimited block to `~/.codex/config.toml` at startup and removes it on exit,
leaving everything else in the file byte for byte. It reads Codex's own
`auth.json` and writes the block that matches your login, API key or ChatGPT plan.
If a session is killed, `kompressor unwrap codex` removes the leftover block.
Details: [docs/CODEX.md](docs/CODEX.md).

A subscription session opens a WebSocket, which the proxy relays and compresses —
on by default. Relayed turns report no provider usage, so their
savings are real but unmeasured; `KOMPRESSOR__PROXY__WEBSOCKET_RELAY=false` trades
that coverage back for the numbers.

With an API key, the proxy can also ask OpenAI to keep each prompt's cache for a
day rather than minutes. It is free to write on gpt-5.5 and earlier, added only
on `api.openai.com` for the models OpenAI documents it for, and dropped again if
the upstream refuses it. Off by default; on archived Codex runs the expiry it
prevents was about 0.35% of the bill
([DESIGN.md §12.10](docs/DESIGN.md#1210-telling-a-live-provider-cache-from-a-lapsed-one)):

```toml
[proxy.providers.openai]
prompt_cache_retention = "24h"
```

### opencode — the TUI and OpenCode Desktop

```bash
kompressor wrap opencode                   # a TUI or `run` session of its own
kompressor wrap opencode --attach          # Desktop, and every session on the background service
```

opencode 2.x is pointed at the proxy through its own configuration, never a
variable: `OPENAI_BASE_URL` in its environment makes every OpenAI model
"unavailable" (measured on 2.2.4). A launched session gets the overlay in
`OPENCODE_CONFIG_CONTENT` and runs `--standalone`, because the shared background
service that otherwise makes the model calls never sees our environment.
Desktop uses that service, so `--attach` writes the overlay into the global
`opencode.json`, runs `opencode reload`, and puts the file back on exit. Both
route the built-in `openai`, `anthropic`, OpenCode Zen and OpenCode Go
providers, and leave alone any provider whose `baseURL` you set yourself.
OpenCode Desktop needs **2.2.4 or newer**: the CLI that 2.0.11 bundles does not
reliably honour a provider's `baseURL`, so its traffic bypasses the proxy.
Details, including a permanent setup: [docs/OPENCODE.md](docs/OPENCODE.md).

### Other agents

```bash
kompressor wrap --list                     # the agents wrap knows
kompressor wrap aider -- --model sonnet    # your own arguments go after --
kompressor wrap --env NAME=root|v1 -- your-agent
```

`wrap` knows `codex`, `claude`, `opencode`, `aider`, `cursor` (prints the base URL
to set in Cursor's settings; only applies when Cursor uses your own OpenAI key) and
`cursor-agent` (refused with an explanation: it speaks Connect RPC over protobuf,
which the proxy cannot read).

<a id="quick-start-claude-code"></a>
### Claude Code

```bash
kompressor wrap claude
```

`wrap claude` uses a kompressor already serving on its port, such as the
installed service, and otherwise starts one with the first config it finds:
`$KOMPRESSOR_CONFIG`, `./kompressor.toml`, then
`~/.config/kompressor/config.toml`. The Anthropic profile comes from that config,
not from the command. The packaged configuration (`/etc/kompressor/config.toml`
on Linux, `~/.config/kompressor/config.toml` on macOS) turns on tail-only
compression and the adaptive one-hour TTL under `[proxy.providers.anthropic]`.
Keep-warm is on by default for Anthropic traffic. It is the one thing the
proxy sends that no client asked for, so to turn it off:

```toml
[proxy.providers.anthropic]
cache_keep_warm = false
```

With no config file at all, Claude Code traffic runs on the deployment defaults,
which freeze the whole conversation Claude Code marks and forward it nearly
untouched. The full profile, set by environment for one session:

```bash
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__RETRIEVE_TOOL=always
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__FROZEN_PREFIX=first_seen
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_TTL=adaptive
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_KEEP_WARM=true
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__ENABLE_CODE_AWARE=false
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__MIN_ELISION_SAVING_CHARS=4000
kompressor wrap claude
```

What Kompressor changes on Claude Code, and why:
[On Claude Code, the cache is the bill](#feature-claude-code-cache). Measured
effect: [Claude Code on Opus 5.5](#results-claude-code).

`wrap claude` also hands Claude Code the retrieve tools as an MCP server it
launches itself, so a `ref:k_…` handle resolves anywhere in the session rather
than only while a request is passing through the proxy. Nothing durable is
written — no `~/.claude.json`, no `~/.claude/settings.json` — so there is no
`unwrap` step. Measurements and the full argument:
[docs/CLAUDE-CODE.md](docs/CLAUDE-CODE.md).

### Claude Desktop — intercepted, because it cannot be pointed

Desktop's **Code tab** is Claude Code, and it cannot be wrapped: the app sets
`ANTHROPIC_BASE_URL` itself when it spawns a session, over the inherited
environment and over `~/.claude/settings.json` alike. There is no value to set.

What it does honour is a proxy and a certificate authority, so that is the way
in:

```toml
[proxy.intercept]
listen = "127.0.0.1:8840"
intercept_hosts = ["api.anthropic.com"]
ca_path = "/home/you/.local/share/kompressor/ca"
alpn = ["h2", "http/1.1"]
```

`CONNECT` on that port, TLS terminated for the allow-listed host with a
certificate minted locally, everything else tunnelled byte for byte. The
decrypted request is served by the same router the proxy already runs, so no
mount or handler changes to support it. The Code tab's traffic is Anthropic
Messages like the CLI's, so the `[proxy.providers.anthropic]` profile described
above applies to it unchanged. Point Desktop at it with `HTTPS_PROXY`
and `NODE_EXTRA_CA_CERTS` in `~/.claude/settings.json`.

The packaged default enables it, so a clean install listens on 8840 and
generates its CA on first start. Nothing trusts that CA until you install it,
and a client that has not been pointed at the proxy is unaffected either way.
Comment out `listen` to turn the listener off — it is off unless an address is
named, so an upgrade of a machine that never had it stays that way.

This covers the Code tab. The **Chat tab** reaches `claude.ai` with a web
session and its own conversation API — nothing a reverse proxy can serve, and
nothing worth serving: its history is held server-side, so a turn carries one
message and an id. Setup, measurements and the limits:
[docs/CLAUDE-DESKTOP.md](docs/CLAUDE-DESKTOP.md). What Chat does carry in bulk
is MCP tool results, and `kompressor mcp serve` can front the servers that
produce them and compress what they return:
[docs/MCP-PROXY.md](docs/MCP-PROXY.md).

### Any client, through the proxy

```bash
kompressor serve                                    # every surface on 127.0.0.1:8817
export OPENAI_BASE_URL=http://127.0.0.1:8817/v1     # OpenAI Chat Completions or Responses
export ANTHROPIC_BASE_URL=http://127.0.0.1:8817     # Anthropic Messages: the root, no /v1
```

### One file, with an explanation

```bash
kompressor compress build.log --explain --show-output
```

`--explain` says why, `--show-output` shows the result with its omission markers
highlighted; both go to standard error, coloured on a terminal, so the payload
on standard output stays pipeable.

Defaults are a working single-tenant setup: no config file, SQLite for fragments
and statistics under `./data`, no authentication, `recoverable` mode. A config
file is read from `$KOMPRESSOR_CONFIG`, then `./kompressor.toml`, then
`$XDG_CONFIG_HOME/kompressor/config.toml`; `kompressor check-config` prints the
effective result with secrets removed.

---

## Results

Every figure is the difference between real provider bills, with the
prompt-cache discount included. Self-reported savings, which price removed tokens
at the uncached list rate, are not used. These are the headline tables; the
method, the per-cohort and per-content-type breakdowns and the caveats are in
[docs/RESULTS.md](docs/RESULTS.md).

<a id="results-codex"></a>
### Codex on GPT-5.5

Terminal-Bench 2.1, Codex CLI on GPT-5.5, one full run over the 87 tasks every
arm completed. Headroom is 0.40 at its defaults. Cost and token savings are
statistically significant; the solve-rate differences are not.

| Per full run          | No compression | Headroom | Kompressor  |
| --------------------- | -------------- | -------- | ----------- |
| Cost                  | $107.89        | $99.04   | **$83.40**  |
| Input tokens          | 96.1M          | 95.5M    | **72.2M**   |
| Uncached input tokens | 6.68M          | 5.16M    | **4.46M**   |
| Tasks solved          | 82.2%          | 81.6%    | 81.1%       |
| Median trial time     | 4.8 min        | 5.3 min  | **4.5 min** |

That is −22.7% against no compression: 2.8× the saving of Headroom's default
(−8.2%) and 2.0× that of its most aggressive setting (−11.6%). The gap sits in
long, log-heavy work; on short tasks neither tool has much to remove.

**Full results:** [docs/RESULTS.md → Codex on GPT-5.5](docs/RESULTS.md#results-codex)
(by task cohort, request sizes, the same traffic replayed through both engines) ·
**Write-up:** [Twice the Savings of Headroom on Coding-Agent Traffic](https://optscale.ai/blog/context-compression-coding-agents)

<a id="results-claude-code"></a>
### Claude Code on Opus 5.5

Terminal-Bench 2.1, 89 tasks, Claude Code on Opus 5.5. The metric is what
Anthropic charges, including keep-warm pings.

| Per 89-task run                    | No compression | Kompressor         |
| ---------------------------------- | -------------- | ------------------ |
| Anthropic API bill                 | $35.35         | **$28.30 (−20%)**  |
| Tasks solved                       | 89.9%          | 90.6%              |
| Cache writes                       | 2.78M tokens   | **1.67M (−40%)**   |
| Prompt tokens lost to cache expiry | 927K           | **13K (−99%)**     |

Cache writes account for $5.59 of the $7.05 difference. Read the total with
care: agents do not take the same path twice, and four tasks account for the
whole net difference. The mechanism-level estimate, which we expect to hold up
across repeated measurements, is about 11% from keeping the cache alive plus a
few percent from compressing the tail.

**Full results:** [docs/RESULTS.md → Claude Code on Opus 5.5](docs/RESULTS.md#results-claude-code)
(where the money went, keep-warm, tasks with and without idle gaps) ·
**Write-up:** [On Claude Code, the Cache Is the Bill](https://optscale.ai/blog/claude-code-prompt-caching)

---

## Features

<a id="feature-cache-safe-compression"></a>
### Cache-safe compression of tool output

Every step a coding agent takes, it re-sends its entire working history to the
model: every apt-get log, every compiler warning, every progress bar. On a long
task the model re-reads the same build output dozens of times, and you pay for
every read.

We built OptScale Kompressor to stop that, for many teams and agents at once.
The same input compresses to the same bytes every turn, so the prompt cache
keeps working, and every removed detail stays one retrieval away.

One rule decides everything else: **the same input compresses to the same bytes,
every turn.** On agent traffic most input tokens are billed as prompt-cache reads,
at roughly a tenth of the fresh price — measured at 93.0% of billed prompt on an
89-task run. A compressor whose output shifts between turns breaks that cache and
costs more than it saves.

On Terminal-Bench 2.1 with Codex on GPT-5.5 it takes 22.7% off the real bill. How
is it different from Headroom, the best-known open-source context compressor,
which has grown into a platform? Kompressor is deliberately just a compressor,
and it saves about twice as much.

**Results:** [Codex on GPT-5.5](#results-codex) · **Write-up:**
[Twice the Savings of Headroom on Coding-Agent Traffic](https://optscale.ai/blog/context-compression-coding-agents)

<a id="feature-claude-code-cache"></a>
### On Claude Code, the cache is the bill

Claude Code marks its conversation with explicit cache breakpoints, so almost
every request is a long cached prefix plus a short new tail: 92–95% of billed
prompt tokens are cache reads. On Opus 5.5 a cache read costs $0.20 per million
tokens and a five-minute cache write $5, 25 times more. A default cache entry
lives five minutes, and a training run, a full build or a long simulation can
keep the agent idle for longer. When it comes back, the whole prefix is written
to the cache again at full price. So on Claude Code the question is less "how
much can we remove?" and more "how much can we avoid paying twice?".

Kompressor's Anthropic profile answers it in three parts:

- **Compress only the uncached tail.** Messages inside the prefix the provider
  has already cached stay byte-identical; only the new tail is compressed, the
  first time it is sent. Elisions under 4,000 characters ship whole, and
  function-body elision is off for this provider, because a retrieval round on
  Claude Code re-sends a large prefix.
- **Keep the cache alive while tools run.** While the agent runs a tool,
  Kompressor re-sends the last request body every 4.5 minutes with
  `max_tokens: 1`, so the provider reads the cached prefix and the entry's
  five-minute clock restarts. Keep-warm stops at the agent's next request, after
  11 pings (about 50 minutes), at the first failed ping, or if a ping reads no
  cached tokens. After a conversation's first idle gap longer than five minutes,
  Kompressor switches it to one-hour cache entries.
- **Price every decision on the real bill:** per-model cache prices, the system
  prompt and tool definitions it never compresses, and Anthropic's 20-block cache
  lookback.

**Results:** [Claude Code on Opus 5.5](#results-claude-code) · **Write-up:**
[On Claude Code, the Cache Is the Bill](https://optscale.ai/blog/claude-code-prompt-caching) ·
**Set-up:** [Quick start → Claude Code](#quick-start-claude-code)

---

## Design rules

**It never changes its mind.** The engine crate has no I/O, no clock and no
randomness. Wherever iteration order could reach the output it uses ordered
containers; handles are content-addressed (`k_` plus a truncated BLAKE2b digest),
so the same fragment gets the same handle on any machine, in any later run. No
compressor may look at the conversation around a payload: ranking a tool result
against the question that prompted it would compress harder and would also
compress differently on every turn it survives into. **274 recorded vectors** pin
which payloads compress and to what, in all three modes.

**It never makes things worse.** Four invariants hold for every payload in every
mode, enforced centrally and checked on the whole corpus:

1. **Never larger.** A compressor that fails to shrink a payload returns the original.
2. **Every shrink is accounted for.** Shorter output carries an omission marker, is
  provably layout-only, or declares a reversible envelope. Anything else is
   refused and the original returned.
3. **Deterministic.** Same input, same bytes, turn after turn.
4. **Fail open.** Errors, panics, timeouts and store outages all degrade to
  forwarding the original. Compression subtracts tokens, never availability.

**Removed is not lost.** In the default `recoverable` mode every elision either
stays behind a handle in a SQLite store or is a pointer to text still on screen.
The proxy offers the model a retrieve tool and executes that tool itself, so
recovery works for a client that knows nothing about OptScale Kompressor.

**No model, no download.** Detection is fifteen ordered rules over the first
megabyte; source code goes through nine tree-sitter grammars compiled into the
binary. Nothing is fetched at runtime and nothing varies between machines.

**One binary.** Library, CLI, HTTP API, MCP server and LLM proxy are the same
binary, in any combination, over one implementation of the algorithm.

---

## Modes


| Mode                        | Guarantee                                                                                                                     |
| --------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `aggressive`                | Lossy. Every elision is announced by a marker; nothing is retained.                                                           |
| `lossless`                  | Only reversible transforms. Nothing is dropped.                                                                               |
| `**recoverable**` (default) | The aggressive engine, with every elision retained behind a handle the model can fetch, or pointing at text still in context. |


Pick per request (`options.mode` on the API or MCP, the `X-Kompressor-Mode` header
on the proxy), per tenant in config, or globally. `off` is accepted at the header
and tenant level: a pass-through that still records statistics. Use `aggressive`
from the bare API if your caller cannot retrieve.

---

## How it works

```
payload -> gutter lift -> redraw fold -> size gate -> detect -+-> segmenter (structured spans in mixed content)
                                                              +-> whole-payload compressor
        -> arbitrate -> accounted guard -> output + handles
           nothing claimed it? clip any opaque over-long line behind a handle
then, across messages: fold runs an earlier message already showed into a pointer
```

- **Detection** is an ordered chain of fifteen rules. The order matters: nearly
every rule exists because a looser one above it claimed payloads it then
compressed badly.
- **Compressors** cover JSON (array sampling, key factoring, nested routing), logs
(template folding and scored selection), grep results, diffs, tables, listings,
source code, configs, and a segmenter that compresses structured spans inside
prose while leaving the prose byte-identical.
- **Cross-message de-duplication** folds a run of lines that an earlier message
already showed into a pointer. It only reads earlier messages, so appending a
turn never rewrites an earlier one, and the pointer needs no handle because the
text is still on screen.
- **Arbitration** computes competing readings of a payload and ships the shorter,
rolling back the loser's retained fragments.

The design and implementation spec is [docs/DESIGN.md](docs/DESIGN.md); diagrams
are in [docs/charts/](docs/charts/).

---

## Surfaces


| Surface    | How                                                                                                                                                                         |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Agent wrap | `kompressor wrap <agent>`                                                                                                                                                   |
| LLM proxy  | `kompressor serve --proxy`: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and a WebSocket relay                                                            |
| HTTP API   | `POST /v1/compress`, `/v1/compress/raw`, `/v1/retrieve`, `/v1/detect`; `GET /v1/stats/*`, `/healthz`, `/readyz`, `/metrics`                                                 |
| Live stats | `GET /v1/stats/live`: JSON, or a self-refreshing dashboard in a browser; on its own port with `server.stats_listen`                                                       |
| Cache      | `GET /v1/stats/cache`: prompt-cache reads, writes and losses per conversation, each loss put down to expiry or a prefix change, and what keep-warm bridged                |
| Dashboards | `GET /v1/stats/{trends,yield,store-health,keepwarm,upstream,engine}`: long-horizon trends, yield by content type, store health, keep-warm return, upstream incidents, engine time; plus a conversation timeline page |
| Ref store  | `GET /v1/refs`: search, inspect and delete stored fragments, as JSON or a page; off unless `refs.browse = true`                                                           |
| MCP server | `kompressor serve --mcp` (streamable HTTP): `optscale_kompressor_retrieve`, `optscale_kompressor_compress`, `optscale_kompressor_compress_raw`, `optscale_kompressor_stats` |
| MCP proxy  | `kompressor mcp serve` (stdio): the same tools, plus those of every `[[mcp.servers]]` it fronts, with their results compressed                                            |
| Backup     | `kompressor backup`, the Maintenance page (off unless `maintenance.backup = true`), or on a schedule (`maintenance.backup_dir`): every database as one `.tar.gz`, service running; each is recorded and Health warns when the last is stale; `kompressor restore` puts one back |
| Databases  | `kompressor db status`, `check`, `checkpoint`, and the same on the Maintenance page: size, write-ahead log, schema version, reclaimable space, integrity; the page also vacuums, optimizes, shows space per table, retention runs and disk headroom, and can trim request settings or delete statistics by day, tenant, run or experiment (preview first), verify archives against their `.sha256`, show what is running and the effective configuration, keep a history of all of it, and erase all statistics (off unless `maintenance.erase = true`) |
| CLI        | `kompressor compress`, `retrieve`, `detect`, `stats`, `bench`, `backup`, `restore`, `db`, `check-config`                                                                                     |
| Library    | the `kompressor-engine` crate: compress a string or a conversation, no I/O                                                                                                  |


`kompressor serve` with no flags runs them all on one listener.

---

## Reading the savings honestly

`saved_gross` is what compression removed. In `recoverable` mode that is not the
whole story: every retrieval puts a fragment back into the prompt and the model
pays for it again, so the figure that matters is
`saved_net = saved_gross - retrieved tokens`, and **it can go negative**. A sum of
gross savings over a window also over-counts on agent traffic, because every turn
re-compresses the same growing history and credits the same bytes again.

`kompressor stats summary` reports both. Before quoting any number, check that
`kompressor_stats_dropped_total` on `/metrics` is zero; otherwise every report is
an undercount. [docs/BENCHMARK.md](docs/BENCHMARK.md) explains how to measure
against a live agent.

The live dashboard, the prompt-cache and analysis pages, sixteen more
dashboards and the JSON route behind each one are described in
[docs/STATISTICS.md](docs/STATISTICS.md). Set
`server.stats_listen = "127.0.0.1:8818"` and open `http://127.0.0.1:8818/` to
watch a session as it happens.

---

## Opt-in settings that edit the prefix

By default Kompressor leaves the client's instructions, tools and headers as it
sent them. These do not, each is off by default, and none has been measured on
live traffic yet: turn one on for a client the `/prefix` page shows needs it.
All are `[proxy]` keys, most also settable per provider;
[DESIGN.md §12.10](docs/DESIGN.md#1210-telling-a-live-provider-cache-from-a-lapsed-one)
has the reasoning for each. The `/advisor` page reads the evidence and says,
per setting, whether it clears the bar.

| Setting | What it does |
| --- | --- |
| `tools_order = "pinned"` | sends a conversation's tools in the order its first request used, when a client reorders them between turns |
| `sticky_beta = true` | re-sends the `anthropic-beta` tokens a conversation already sent when a later request leaves some out |
| `prompt_cache_key = "conversation"` | names an OpenAI cache route from the conversation's opening where the client named none |
| `tools_compact = true` | drops schema bookkeeping from tool definitions (`$comment`, a `title` that restates its property's name, a `$schema` no keyword in the schema depends on); about 2% of the tool block on Claude Code |
| `cold_recompact = "observed"` or `"predicted"` | under `frozen_prefix = "first_seen"`, rewrites the whole conversation on the turn that finds the provider's cache gone, and replays what that turn decided on every turn after it |
| `superseded_reads`, `matured_reads` | with `cold_recompact`: on that turn, also replace a file read a later read contains, and compress as source a read left alone for `matured_quiesce_turns` |
| `reasoning_elide_models = ["…"]` | OpenAI Chat: for the models named, elides the plain-text reasoning of turns before the last user message. Rewrites the finished turn once per user turn |

---

## Not built yet

- **Needle retention**: a measure of whether compression kept what the agent went
on to use. It is the one quality metric that can fail while every ratio improves.
- **Redis fragment backend**: needed for several proxies behind one load balancer.
SQLite and in-memory stores exist.
- `**bench` subcommands** beyond `replay` and `extract` (`sample`, `needles`,
`calibrate-tokens`).
- **Per-stage tracing and proxy metrics on `/metrics`**: status codes, fail-open
stages and retrieve rounds are in `stats.db` today.
- **A retrieve loop over the WebSocket relay.** Relayed turns are compressed, but
the tool cannot be offered on that transport; fragments come back by piggyback on
the following frame instead.
- `X-Kompressor-Retrieve-Rounds` is always 0 on streamed responses, because headers
precede the loop; the real count reaches `stats.db`.
- An elision larger than `max_ref_bytes` (default 1 MiB) is declined in
`recoverable` mode, and the payload is forwarded whole.

Good first issues are labelled
`[good first issue](https://github.com/hystax/optscale-kompressor/labels/good%20first%20issue)`.

---

## Development

`make` on its own lists every target. The ones that matter:


|                     |                                                                                                |
| ------------------- | ---------------------------------------------------------------------------------------------- |
| `make check`        | what CI gates on: lint, tests, the corpus replay against its recorded baseline, the docs check |
| `make bench`        | replay the committed corpus, compare against `fixtures/baseline.json`, verify determinism      |
| `make smoke`        | start every surface in a scratch directory, exercise it end to end, stop it                    |
| `make e2e`          | the hermetic suite: three containers, all modes, refs and the retrieve loop                    |
| `make harbor-gates` | every live-agent benchmark gate, with no provider spend                                        |


`cargo test --workspace` runs **1,882 tests** across ten crates; clippy and rustfmt
are clean. `make bench-save` and `make golden` move what the tests compare against,
so both ask before they run.

`make e2e` reports `all good` or names what failed; it has never printed a total,
so no check count is quoted here.

---

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
Commits need a DCO sign-off (`git commit -s`). Please report security issues
privately as described in [SECURITY.md](SECURITY.md), not in public issues.
Everyone taking part is expected to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Acknowledgements

We took the base idea from [Headroom](https://github.com/headroomlabs-ai/headroom)
(Apache-2.0): routing a payload to a compressor written for its content type, the
compress-and-retrieve pattern, the taxonomy of why a request was not compressed,
and the code body-budget idea. Headroom also found the two Claude Code base-URL
side effects that `wrap claude` works around. The implementation here is our own.

## Built by Hystax

OptScale Kompressor is developed by [Hystax](https://hystax.com) and is the
compression engine inside [OptScale AI](https://optscale.ai), which runs it across
an organisation with shared savings dashboards, per-team policy and governance. The
engine and everything in this repository stay MIT-licensed.

## License

MIT — see [LICENSE](LICENSE).
