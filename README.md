# OptScale Kompressor

![CI](https://github.com/hystax/optscale-kompressor/actions/workflows/ci.yml/badge.svg)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![MSRV 1.95](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

**Deterministic prompt compression for coding agents, in one Rust binary.**

OptScale Kompressor sits between a coding agent and its model provider. It
removes the repetitive tool output agents re-send on every turn, and on Claude
Code it keeps the provider's prompt cache alive while tools run. Each feature has
its own write-up and its own results:

| Feature | Measured on | Bill vs no compression | Results |
| --- | --- | --- | --- |
| [Cache-safe compression of tool output](#feature-cache-safe-compression) | Codex on GPT-5.5 | **−22.7%**, about twice Headroom's savings | [Codex results](#results-codex) |
| [On Claude Code, the cache is the bill](#feature-claude-code-cache) | Claude Code on Opus 5.5 | **−20%**, with 99% fewer tokens lost to cache expiry | [Claude Code results](#results-claude-code) |

```bash
kompressor wrap codex
```

That starts the proxy, points Codex at it, runs Codex, and cleans up on exit.
When the session ends it tells you what happened:

```console
kompressor: 26 request(s), 126 message(s) compressed, ~2029706 tokens saved (2015180 net of retrieval)
```

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

## Results

Every figure below is the difference between real provider bills, with the
prompt-cache discount included. Self-reported savings, which price removed tokens
at the uncached list rate, are not used. Each feature has its own section:

- [Codex on GPT-5.5](#results-codex): cache-safe compression of tool output
- [Claude Code on Opus 5.5](#results-claude-code): the cache is the bill

<a id="results-codex"></a>
### Codex on GPT-5.5

**Feature:** [Cache-safe compression of tool output](#feature-cache-safe-compression) ·
**Full write-up, with the task-by-task appendix:**
[Twice the Savings of Headroom on Coding-Agent Traffic](https://optscale.ai/blog/context-compression-coding-agents)

**On Terminal-Bench 2.1.** 89 tasks, Codex CLI on GPT-5.5 ($5.00 / $0.50 / $30.00 per million uncached / cached / output tokens), Harbor with 3 concurrent trials. Four arms: no compression; Headroom 0.40 at its defaults, with its ML model; Headroom at its most aggressive setting (retrieval off, a 120-token size floor, at most 8 array items); OptScale Kompressor via OptScale AI. The table is one full run over the 87 tasks every arm completed. Trials that failed agent setup for external reasons are excluded. Cost and token savings are statistically significant; the solve-rate differences are not.


| Per full run          | No compression | Headroom | Kompressor  |
| --------------------- | -------------- | -------- | ----------- |
| Cost                  | $107.89        | $99.04   | **$83.40**  |
| Input tokens          | 96.1M          | 95.5M    | **72.2M**   |
| Uncached input tokens | 6.68M          | 5.16M    | **4.46M**   |
| Tasks solved          | 82.2%          | 81.6%    | 81.1%       |
| Median trial time     | 4.8 min        | 5.3 min  | **4.5 min** |


Headroom in that table is the default. Against the same baseline its most
aggressive setting saved 11.6%.


| Against no compression            | Cost       | Solved |
| --------------------------------- | ---------- | ------ |
| Kompressor                        | **−22.7%** | 81.1%  |
| Headroom, most aggressive setting | −11.6%     | 85.1%  |
| Headroom, default                 | −8.2%      | 81.6%  |


Kompressor's saving is 2.0× Headroom's most aggressive setting and 2.8× its
default. Head to head with the default, Kompressor is 15.8% cheaper, sends 24.4%
fewer input tokens and 13.6% fewer uncached input tokens, and solves 0.5 points
fewer tasks (81.1% vs 81.6%). It was cheaper on 54 of 87 tasks. The gap sits in
long, log-heavy work — ML training, cross-compilation, statistical modelling. On
short tasks neither tool has much to remove.

**By task cohort.** Removed is the share of eligible tool output (at least 512
characters) taken out. ML training, source builds and data pipelines produce 83%
of the total saving. Compression is weakest where outputs are tiny, and where
they are prose that should not be folded.


| Task cohort                                     | Tool output per task | Removed | Bill vs no compression |
| ----------------------------------------------- | -------------------- | ------- | ---------------------- |
| ML training, sampling and evaluation (17 tasks) | 598K chars           | **81%** | −26%                   |
| Reverse engineering and security (12)           | 310K chars           | **69%** | −5%                    |
| Building software from source (10)              | 1,178K chars         | **64%** | −28%                   |
| Service configuration and git (10)              | 262K chars           | **58%** | −14%                   |
| Data pipelines and media (9)                    | 288K chars           | **55%** | −37%                   |
| Text, documents and sequences (7)               | 120K chars           | **51%** | −23%                   |
| Algorithms, maths and puzzles (22)              | 99K chars            | **48%** | −8%                    |


**Smaller requests.** A cached token is still sent, and it still fills the
context window. Byte sizes are estimated at 4.35 bytes of request JSON per token,
measured on the no-compression runs.


|                           | No compression          | Headroom         | Kompressor          |
| ------------------------- | ----------------------- | ---------------- | ------------------- |
| Prompt sent per run       | 96.1M tokens · ~418 MB  | 95.5M · ~415 MB  | **72.2M · ~314 MB** |
| 90th-percentile request   | 106.9K tokens · ~465 KB | 110.5K · ~481 KB | **66.5K · ~289 KB** |
| Requests over 100K tokens | 11.2%                   | 12.0%            | **3.2%**            |
| Requests over 200K tokens | 1.9%                    | 0.6%             | **0.13%**           |


Without compression, 1.9% of requests passed 200K of GPT-5.5's 258K window, and
two trials compacted their context. With Kompressor, 0.13% did, and one trial
compacted. On the heavy tasks, `mcmc-sampling-stan` peaked at 60K tokens instead
of 165K, and `caffe-cifar-10` at 68K instead of 152K. Where a provider does not
cache, the 25% cut in prompt traffic goes straight to the bill.

**Replayed on the same traffic.** The same 6,255 tool outputs (34.2M characters)
through three engines at default settings: optscale-kompressor 0.13, Headroom
0.40 with its ML model off, and Headroom 0.40 with Magika detection plus its
ModernBERT model.


| Content type                 | Share    | Kompressor | Headroom, ML on | Headroom, ML off |
| ---------------------------- | -------- | ---------- | --------------- | ---------------- |
| Listings and dumps           | 27.8%    | **81.8%**  | 22.6%           | 7.0%             |
| Plain text                   | 25.3%    | **41.0%**  | 20.6%           | 5.2%             |
| Build output and logs        | 19.3%    | **75.4%**  | 17.1%           | 4.2%             |
| Source code                  | 13.8%    | **35.0%**  | 31.9%           | 0.0%             |
| Search results               | 11.1%    | **82.7%**  | 25.5%           | 25.4%            |
| Diffs, config, HTML, JSON    | 2.7%     | **40.9%**  | 19.8%           | 12.5%            |
| **All eligible tool output** | **100%** | **62.8%**  | 22.6%           | 7.3%             |
| Time per KB                  |          | **0.2 ms** | 87 ms           | 1.5 ms           |


Kompressor removes 2.8× as much as Headroom with its ML model on, and 8.6× as
much as Headroom without it, in about 1/440 of the time the model needs. Every
removed detail stays retrievable.

<a id="results-claude-code"></a>
### Claude Code on Opus 5.5

**Feature:** [On Claude Code, the cache is the bill](#feature-claude-code-cache) ·
**Full write-up:**
[On Claude Code, the Cache Is the Bill](https://optscale.ai/blog/claude-code-prompt-caching)

**On Terminal-Bench 2.1.** 89 tasks, Claude Code on Opus 5.5 ($4.00 / $0.20 / $5.00 / $8.00 / $20.00 per million uncached input / cache read / five-minute cache write / one-hour cache write / output tokens). Two arms: no compression, and OptScale Kompressor with the Anthropic profile. The metric is what Anthropic charges: every API request, including keep-warm pings and Claude Code's calls to other models. Harbor's own cost sees only the requests in the agent's trajectory; without compression it reported $32.00 against $35.35 billed. Trials that failed agent setup are excluded.


| Per 89-task run                    | No compression | Kompressor         |
| ---------------------------------- | -------------- | ------------------ |
| Anthropic API bill                 | $35.35         | **$28.30 (−20%)**  |
| Tasks solved                       | 89.9%          | 90.6%              |
| Prompt tokens served from cache    | 92.0%          | **94.8%**          |
| Cache writes                       | 2.78M tokens   | **1.67M (−40%)**   |
| Prompt tokens lost to cache expiry | 927K           | **13K (−99%)**     |
| Output tokens                      | 660K           | 596K               |
| Agent steps                        | 1,104          | 1,080              |


Individual Kompressor runs cost between $27.86 and $28.68 and solved 80–81 tasks.

**Where the money went.** Cache writes account for most of the difference:
$5.59 of $7.05. The extra spend on pings and one-hour entries comes to under a
dollar, about a sixth of what the writes saved.


| Per run                | No compression | Kompressor | Change     |
| ---------------------- | -------------- | ---------- | ---------- |
| Cache writes, 5-minute | $14.16         | $8.57      | **−$5.59** |
| Cache writes, 1-hour   | —              | $0.14      | +$0.14     |
| Cache reads            | $7.37          | $6.46      | −$0.91     |
| Output                 | $13.60         | $12.31     | −$1.29     |
| Uncached input         | $0.23          | $0.09      | −$0.14     |
| Keep-warm pings        | —              | $0.73      | +$0.73     |
| **Total**              | **$35.35**     | **$28.30** | **−$7.05** |


**Keep-warm.** Each bridged gap would otherwise have ended in a full rewrite of
the prefix, which is what the uncompressed agent paid: 927K tokens per run. Net
of the pings, keep-warm saved about $3.9 per run, roughly 11% of the
uncompressed bill, on its own. In most runs no turn lost its cache at all.


| Keep-warm, per run                                   | Average | Range        |
| ---------------------------------------------------- | ------- | ------------ |
| Pings sent                                           | 93      | 73–112       |
| Idle gaps over five minutes bridged                  | 28      | 22–35        |
| Prefix tokens kept warm                              | 0.96M   | 0.78M–1.20M  |
| Cost of the pings                                    | $0.73   | $0.46–0.89   |
| Rewrites avoided, at $5 instead of $0.20 per million | ~$4.6   | ~$3.7–5.7    |


**Reading it honestly.** Agents do not take the same path twice. Four tasks
account for the whole net difference: `winning-avg-corewars`, `regex-chess`,
`sam-cell-seg` and `crack-7z-hash`. On the first three, the uncompressed agent
took a much longer path: 45–72 steps against 19–26. Kompressor was cheaper on 44
of the 89 tasks. Splitting tasks by whether they wait on long-running tools
separates the two effects:


| Tasks                                                                | Count | Cost     | Median cost per step |
| -------------------------------------------------------------------- | ----- | -------- | -------------------- |
| With idle gaps over five minutes (training, builds, long simulations) | 14    | **−34%** | **−20%**             |
| All others                                                           | 72    | −12%     | +2%                  |


Where the cache would expire, keep-warm and the adaptive TTL make each step
cheaper. On a typical short task a step costs what it did before, and the rest of
that cohort's saving comes from shorter paths. The mechanism-level estimate,
which we expect to hold up across repeated measurements, is about 11% from
keeping the cache alive plus a few percent from compressing the tail.

**What compression still does on Claude Code.**


| Per run, average                              | Value                                   |
| --------------------------------------------- | --------------------------------------- |
| Tokens removed from the uncached tail         | 348K (4.7–8.0% of what the engine saw)  |
| Messages protected inside the cached prefix   | ~1,800, about 2.2M tokens               |
| Elisions with a retrieval handle              | 126                                     |
| Retrievals the model made                     | 0–2 per run, every one a hit            |


Removal is far below the 62.8% in the [Codex replay](#results-codex), by design:
the engine only sees the uncached tail, skips elisions under 4,000 characters,
and leaves code bodies alone on this provider. The model almost never needs to
retrieve anything, so compression adds no retrieval cost of its own.

---

## Install

Pre-built binaries for Linux (x86_64, arm64) and macOS (arm64, x86_64), with
checksums, are on the [Releases page](https://github.com/hystax/optscale-kompressor/releases/latest).

With Cargo (Rust 1.95 or newer):

```bash
cargo install --locked --git https://github.com/hystax/optscale-kompressor kompressor-cli
```

From source:

```bash
git clone https://github.com/hystax/optscale-kompressor
cd optscale-kompressor
make release
./target/release/kompressor version
```

`make deb`, `make rpm`, `make static` (musl), `make docker`,
`make macos-pkg-docker`, `make win` and `make win-installer` build packages; each
says what it needs if a tool is missing. [docs/QUICKSTART.md](docs/QUICKSTART.md) covers running it as a service,
[docs/MACOS.md](docs/MACOS.md) covers the macOS paths, and
[docs/WINDOWS.md](docs/WINDOWS.md) the Windows ones, including Claude Desktop
and the Codex app.

---

## Quick start

<a id="quick-start-install"></a>
### Install a prebuilt package

No Rust toolchain needed: download the package for your platform from the
[latest release](https://github.com/hystax/optscale-kompressor/releases/latest)
and check it against `SHA256SUMS` from the same page
(`sha256sum --check --ignore-missing SHA256SUMS`).

| Platform | File | Install |
| --- | --- | --- |
| Debian, Ubuntu | `kompressor_<version>-1_amd64.deb` | `sudo apt install ./kompressor_*-1_amd64.deb` |
| RHEL, Fedora | `kompressor-<version>-1.x86_64.rpm` | `sudo dnf install ./kompressor-*-1.x86_64.rpm` |
| Any Linux x86_64 | `kompressor-<version>-x86_64-linux-musl` (static) | `install -m 755 kompressor-*-x86_64-linux-musl ~/.local/bin/kompressor` |
| macOS (Apple silicon and Intel) | `kompressor-<version>-universal2.pkg` | `sudo installer -pkg kompressor-*-universal2.pkg -target /` |
| Windows x86_64 | `kompressor-<version>-x86_64-setup.exe` | run it; see [docs/WINDOWS.md](docs/WINDOWS.md) |

Older glibc systems can use the `-static` `.deb` / `.rpm` builds instead. With
the [GitHub CLI](https://cli.github.com) the download is one command, for
example:

```bash
gh release download --repo hystax/optscale-kompressor -p 'kompressor_*-1_amd64.deb' -p SHA256SUMS
```

Then check it runs:

```bash
kompressor version
```

To build from source instead, see [Install](#install).

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
on by default since 0.14.0. Relayed turns report no provider usage, so their
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

### From your own code: the thin clients

`clients/python` (standard library only) and `clients/typescript` (`fetch`
only) wrap `/v1/compress`, `/v1/compress/raw`, `/v1/retrieve` and
`/v1/detect` of a running `kompressor serve`, with a fail-open
`compress_or_passthrough`. No second engine: the server does the work.

```python
from kompressor_client import Kompressor
messages = Kompressor("http://127.0.0.1:8817").compress_or_passthrough(messages)
```

### Gemini, Vertex AI and Bedrock

Besides the OpenAI and Anthropic APIs, the proxy compresses Gemini
`generateContent` (Google AI and Vertex), Claude on Vertex (`:rawPredict`),
Claude on Bedrock (`/model/{id}/invoke`) and Bedrock Converse, including
Gemini `functionResponse` tool output. A Bedrock mount with `aws_sigv4`
re-signs every request over the compressed body. Paths, auth and what differs
(no in-band retrieve tool where the response is not read): 
[docs/CLOUD-PLATFORMS.md](docs/CLOUD-PLATFORMS.md).

### Other agents

```bash
kompressor wrap --list                     # the agents wrap knows
kompressor wrap aider -- --model sonnet    # your own arguments go after --
kompressor wrap --env NAME=root|v1 -- your-agent
```

`wrap` knows `codex`, `claude`, `opencode`, `copilot` (BYOK), `goose`, `aider` and
`cursor-agent`. `cline` and `continue` are configured in the editor:
`kompressor wrap cline` prints the base URLs to paste and holds the proxy up
until Ctrl-C.

### Is it working? `kompressor doctor`

```bash
kompressor doctor          # one line per check, a remedy under each problem
kompressor doctor --json   # {version, exit_code, checks: [{check, status, detail, remedy}]}
```

It checks the configuration, whether a kompressor answers on the proxy
listener, the upstreams, the fragment and statistics stores (Redis is pinged),
which known agents are on `PATH`, and every `ANTHROPIC_BASE_URL`-style variable
in the shell that points at a loopback port nothing answers on. It never calls
a provider. Exit 0 healthy, 1 warnings only, 2 a failure.

### Cursor

Cursor's traffic is protobuf to its own servers, which no proxy can read, so it
is compressed through Cursor's own hooks instead — no proxy runs:

```bash
kompressor wrap cursor-agent -- -p "fix the failing test"   # one console session
kompressor hook install cursor --shadow                      # editor and CLI: measure first
kompressor hook install cursor                               # then compress MCP results
kompressor hook status cursor
```

MCP tool results are compressed; shell output is opt-in; built-in Read and Grep
output cannot be. How much this saves on Cursor has not been measured yet —
Cursor already keeps any tool result over ~40 KB out of the conversation, and
shadow mode is how to find out on your own work. [docs/CURSOR.md](docs/CURSOR.md).

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
| CLI        | `kompressor compress`, `retrieve`, `detect`, `stats`, `bench`, `backup`, `restore`, `db`, `check-config`, `doctor`                                                                           |
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

To watch it as it happens, set `server.stats_listen = "127.0.0.1:8818"` and open
`http://127.0.0.1:8818/`: a dashboard of the last hour (or 5 minutes, or 7
days) — throughput, tokens removed, net of retrieval, cache reads, upstream
status and latency, savings by provider, model and content type, why messages
were left alone, and the latest requests — refreshed every two seconds. The
same data is JSON at `/v1/stats/live`. It shows the dropped-event count too.

`http://127.0.0.1:8818/cache` is the provider's prompt cache over the last six
hours (or one, or 30 days): how much of the input was read from cache, what
the cache saved against no cache at all, every turn that failed to read back
the prefix the turn before left — put down to expiry when the conversation
had idled past the cache lifetime, to a prefix change otherwise — the idle
gaps before each turn, and what the keep-warm pings cost and bridged. Click a
conversation for its turns one by one. The JSON is `/v1/stats/cache`.

`http://127.0.0.1:8818/analysis` analyzes a fixed date range: conversation
compression credits and retrieval debits, provider usage, cache losses, latency,
and errors, plus compression outcomes and skipped categories ranked by token
volume. Open a conversation for its turn timeline, with links to cache details
and retrieved fragments (when ref browsing is enabled). The JSON endpoint is
`/v1/stats/analysis?from=<epoch-ms>&to=<epoch-ms>`; `to` is exclusive, ranges
span at most 90 days, and `conversation=<fingerprint>` adds turn details.
Both `/v1` and `/v2` routes serve HTML with `?format=html`. Cache deep links also
accept the same `from`/`to` pair. Analysis uses retained raw events; daily
archives cannot reconstruct turn history. Token reduction remains an estimate
per request, not a monetary saving or a count of unique content.

Sixteen more pages sit beside those on the statistics listener, linked from each
other and from every page's **Dashboards** link:

- `/health`: one verdict for the last hour (or any window) — upstream incidents
  (429, 5xx, no answer) and failure bursts, refused (4xx) and retried requests,
  engine p95 and slots busy now, retrieval misses and budget-withheld asks, ref
  store fill against its cap, and dropped statistics — each judged ok, warn or
  bad against a fixed threshold and linked to the dashboard that explains it.
  The Live page links to it.
- `/trends`: weekly (or daily, or monthly) net tokens removed, prompt-cache
  saving, input cost, conversations and errors for up to two years. It reads the
  daily archive as well as raw rows, so it reaches past the raw-row retention,
  and marks the period each build was first seen.
- `/yield`: what each content type gave up, and what retrieval took back by
  fragment kind (how many stored fragments were ever asked for, and how soon).
  The engine records messages per type rather than tokens, so the per-type
  saving is an estimate; runs made of a single type give the exact figure.
- `/timeline`: one conversation drawn turn by turn — what the cache read, wrote
  and charged fresh, each lost prefix, the pings, compressions and retrievals.
- `/store`: what the ref store holds by kind, age and time to expiry, against
  its byte cap, how many minted handles were ever asked for, and asks that came
  too late (expired) or for nothing (missing).
- `/keepwarm`: what the keep-warm pings cost against the rewrites they avoided,
  by pause length and per conversation, with the pings sent after a
  conversation's last turn and the TTL losses no ping covered.
- `/cache-loss`: whether compression costs the prompt cache more than it
  saves. Compared client turns are split into those whose engine run rewrote
  messages, those it ran on and left alone, and those it never saw; the
  prefix-change loss rate of the untouched turns is the baseline, losses on
  compressed turns above it are the *excess* (the estimate), every loss on a
  compressed turn the upper bound, and a loss after the freeze was lifted (the
  run froze fewer messages than the conversation's previous one) the *likely*
  case. TTL losses are left out. The excess cost is set against what the
  removed tokens were worth at cache-read rates, the least they saved.
- `/upstream`: status classes, 429s, 5xx and unanswered requests by upstream
  and model, latency percentiles, incidents (three or more such failures within
  a minute of each other) and the latest failures.
- `/traffic`: what the proxy relays — model calls, keep-warm pings, WebSocket
  upgrades and pass-through — and each row's outcome: expected (2xx, 304, 101,
  redirects), client side (other 4xx, client disconnects) or upstream (429,
  5xx, no answer, a broken stream). It shows the real error share (upstream
  failures of model calls and pings) beside the naive one a status-class count
  would report, by upstream, API family and status, with the error messages.
- `/engine`: where compression time goes, by content type, prompt size, outcome
  and surface, the slowest runs, the engine slots in use and dropped stats events.
- `/quality`: how far the other pages can be trusted — the share of rows that
  carry provider usage, a conversation, upstream timing and run context; engine
  runs with an outcome and skip reasons; retrievals tied to a request; requests
  filed with more than one run; dropped statistics, a window reaching past
  retention, the archive's last day, and tables too busy to read whole.
  Columns this build never writes (such as the uncompressed-prompt measurement)
  are shown as information, not as failures.
- `/models`: one row per API family and model, putting side by side what is
  otherwise on four pages — what the provider billed and how much of it was
  cache reads, what the engine removed and how long it took, what retrieval put
  back, and how often the upstream failed. Where compression pays, and where it
  only costs latency.
- `/activity`: the window folded onto one week, a cell per weekday and hour in
  your browser's zone, shaded by model calls, billed input, cache writes, engine
  savings or keep-warm pings; the busiest hour and the busiest eight hours.
- `/cohorts`: conversations grouped by how many client turns they ran (1, 2–4,
  5–19, 20–99, 100–499, 500+), with each cohort's share of the cost, its cache
  economics and its net compression saving, and how few conversations carry
  half the bill.

- `/prefix`: which part of the cacheable prefix moved between two turns of a
  conversation, and on which side of the proxy. Each request is reduced to a
  hash per segment — beta headers, the tool set, tool order, tool definitions,
  the system prompt, earlier messages — over the body as the client sent it
  and as it went upstream, and compared with the turn before. Every
  prefix-change loss on the Cache page is put down to the earliest segment
  that changed and to the client or the proxy, or listed as unexplained. The
  page also shows what a request is made of per model (tools, system,
  reasoning and the conversation the engine cannot see, in bytes), volatile
  values in instructions and tool descriptions (dates, date-times, UUIDs,
  JWT-shaped tokens), and how well a cache lapse can be predicted on the
  request that finds it, per lifetime and margin. Observation only: no byte of
  any request changes.

- `/advisor`: which of the opt-in settings would pay on this traffic. One row
  per setting — pinned tool order, kept beta tokens, an OpenAI cache key,
  schema compaction, recompaction on a lapsed cache, the read decisions,
  reasoning elision, and tool deferral — with whether it
  is on now, a verdict (turn on, not worth it, not enough data), the value to
  set, the figure the verdict rests on beside the bar it was held against, a
  ceiling on the saving, and the reason in a sentence. Under it, the figures
  behind each row and what the settings that are on have actually done. The
  bars are the design's proposals, printed with each verdict, not measured
  optima; three of the figures (what compaction would remove, what a
  recompaction would rewrite, whether a client names its own cache key) are
  measured on every request without acting on them.

### Statistics JSON API for automation

Every dashboard is backed by a JSON route, and a client that sends no `Accept`
header, `*/*` or `application/json` always gets JSON (`?format=json` forces it
even for a browser). The same routes are served under `/v2`, and on the
statistics listener (`server.stats_listen`) as well.

| Route | Answers |
|---|---|
| `GET /v1/stats/live?window=1h&recent=25` | the live dashboard's snapshot: totals, rates, timeline, upstream, by provider/model/type, skips, retrievals, recent requests, process info |
| `GET /v1/stats/cache?window=6h` or `?from=&to=` | the cache dashboard's report; `conversation=<16 hex>` adds that conversation's turns |
| `GET /v1/stats/analysis?from=&to=` | the analysis overview (`report`, `stats_dropped`, `retention_days`); defaults to the last 24 h |
| `GET /v1/stats/analysis/retrievals?from=&to=` | retrieval amplification |
| `GET /v1/stats/analysis/latency?from=&to=` | latency and reliability |
| `GET /v1/stats/analysis/compare?from=&to=` | historical comparison; `compare_from`/`compare_to`, `model`, `mode`, `surface`, `experiment_id`, `run_id`; `output_shaping` sets the two arms of `proxy.output_shaping` against each other within the window |
| `GET /v1/stats/trends?window=26w&bucket=week` | per-period history from raw rows and the daily archive; `window` up to `104w`, `bucket` `day`, `week` or `month`, or `from=&to=` up to 730 days |
| `GET /v1/stats/yield?window=24h` | tokens removed per content type (estimated by message share) and retrieval asks per fragment kind |
| `GET /v1/stats/store-health?window=24h` | the ref store's census by kind, age and expiry, its size over time, minted versus asked handles, expired and missing asks |
| `GET /v1/stats/keepwarm?window=24h&conversations=50` | keep-warm ping cost against avoided rewrites, by pause length and per conversation |
| `GET /v1/stats/health?window=1h` | ok / warn / bad verdict with each judged check, its thresholds and the dashboard behind it, a request/failure timeline, recent incidents and failures |
| `GET /v1/stats/cache-loss?window=24h&conversations=50` | prefix-change losses by what the engine did to the turn, baseline rate, excess / likely / upper-bound losses with their cost, removed tokens at cache-read rates, conversations and the latest suspect losses |
| `GET /v1/stats/traffic?window=24h` | rows by class and outcome, real vs naive error share, noise share, by upstream / API family / status, grouped error messages, a timeline |
| `GET /v1/stats/quality?window=24h` | coverage checks (ok / warn / bad, or info for columns not written), rows per table in the window, the oldest raw row, retention and the last archived day |
| `GET /v1/stats/upstream?window=24h` | status mix, latency and failures by upstream and model, incidents, latest failures |
| `GET /v1/stats/engine?window=24h` | engine time by content type, prompt size, outcome and surface, slowest runs, process slots |
| `GET /v1/stats/models?window=24h` | per API family and model: client requests and pings, billed input split by how it was billed, engine runs, reduction and latency percentiles, retrieval rounds and tokens put back, net saving, upstream failures |
| `GET /v1/stats/activity?window=7d` | requests, pings, pass-through, billed input, cache writes and engine savings per weekday and hour (UTC), the busiest cell and the busiest eight consecutive hours |
| `GET /v1/stats/cohorts?window=7d` | conversations bucketed by client turns, each with its billed input, cost share, net saving and median length, and the share of conversations that carries half the cost |
| `GET /v1/stats/prefix?window=24h` | compared requests and how many moved nothing; per prompt segment the turns the client changed it and the turns only the proxy did, with the prefix-change losses, tokens and cost put down to each; losses the observations cannot explain; request composition per model; volatile values in instructions; predicted-cold precision and recall per lifetime and margin |
| `GET /v1/stats/advisor?window=24h` | one `advice` row per opt-in setting: `verdict` (`turn_on`, `not_worth_it`, `not_enough_data`), `value` to set, `enabled` now, `built`, the `measure` and `threshold` with what they are a share of, a `saving` ceiling in input-equivalent tokens, `why`, and the `facts` behind it; plus `effects`, what the settings that are on did in the window |
| `GET /v1/stats/summary`, `histogram`, `by_type`, `retrievals`, `requests?request_id=`, `store` | the aggregate reports `kompressor stats` prints |

`from`/`to` are epoch milliseconds (`to` exclusive, at most 90 days apart).
Authenticate with `x-kompressor-key` (or `Authorization: Bearer`); a key reads
only its own tenant, and a caller holding the cluster secret
(`x-kompressor-secret`) may name any tenant with `?tenant=`. Unknown query
parameters are refused with `422`, and every error uses the same
`{"error":{"code","message"}}` envelope. With statistics off, the routes answer
`404` saying so.

```bash
curl -s -H 'x-kompressor-key: sk-…' "http://127.0.0.1:8840/v1/stats/analysis/latency?from=$(( ($(date +%s) - 3600) * 1000 ))&to=$(( $(date +%s) * 1000 ))" | jq .total
```

Every dashboard has a **Download JSONL** button, and each of their JSON
routes — `/v1/stats/live`, `/v1/stats/cache`, `/v1/stats/analysis` and its
`retrievals`, `latency` and `compare` views, and the six dashboard routes above —
also answers `?format=jsonl` with a JSON Lines attachment, under the same key
and tenant rules as the JSON. The first line is `{"record":"summary",…}` with
every field that is not a list; each list element follows on its own line,
tagged with the list's dotted path, e.g. `{"record":"timeline",…}` or
`{"record":"report.conversations",…}`:

```bash
curl -H 'x-kompressor-key: sk-…' 'http://127.0.0.1:8840/v1/stats/cache?window=24h&format=jsonl' > cache.jsonl
```

The Analysis page also includes retrieval amplification (negative-net requests,
repeated reference asks, ranged/full retrieval volume, and budget refusals),
historical comparison, and latency/reliability tabs. Their JSON endpoints are
`/v1/stats/analysis/retrievals`, `/v1/stats/analysis/compare`, and
`/v1/stats/analysis/latency`, each accepting `from`/`to` and the existing tenant
credentials. They are also available under `/v2`.

Comparison accepts `compare_from`/`compare_to` for the baseline (default: the
preceding equal-length window) and exact `model`, `mode`, `surface`,
`experiment_id`, and `run_id` filters applied to both periods. It reports totals,
per-client/per-conversation values, absolute differences, and relative changes;
zero denominators remain unavailable. To label a benchmark, set optional
`stats.experiment_id` and `stats.run_id`. Otherwise each AppCore instance generates
its own run ID. Events also carry the build version and a canonical fingerprint
of an explicit allowlist of deployment compression/cache/retrieval settings,
including provider policy overrides. Credentials and payloads are excluded.
Actual compression mode is recorded separately; other per-request overrides
are not separate configuration fingerprints.

Schema v13 preserves daily experiment totals and conversation fingerprint sets
before retention deletes raw events. Late arrivals are added once, and archived
and raw totals can be combined. Archived comparisons require complete UTC days;
partial archived days are explicitly omitted. Older legacy daily rollups lack
experiment dimensions and are excluded from this comparison. Turn ordering,
retrieval rankings, and latency percentiles still require retained raw events.

New HTTP requests record upstream response-head wait for each attempt (including
retries/failures), request-start-to-first-response-head time, and local retrieval
answer/refusal execution time. Attempt detail is capped at 64 with the total
count retained. This measures headers, not time to first token, and phases
must not be added together. Latency reports exclude keep-warm pings, preserve
missing sample counts, and show distributions, model/round/prompt-size/status
breakdowns, slow requests, and error trends. Reports over more than 200,000 client
rows explicitly use the newest 200,000. Detailed timing is unavailable for old
rows and the WebSocket relay. Comparisons are observational and do not measure
task-quality preservation or prove causation.

### Opt-in settings that edit the prefix

Everything above leaves the client's instructions, tools and headers as it
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
| `tools_defer = true` | first-party Anthropic: marks every tool but the core ones (`tools_defer_keep`) `defer_loading` and adds Anthropic's tool-search tool, for requests with at least `tools_defer_min_tools` (12) tools whose client does not defer already; decided per conversation on its first request |
| `output_shaping = true` | appends a fixed "answer concisely" instruction ahead of the conversation — after the last `system` block (Anthropic), at the end of `instructions` (OpenAI Responses), or to the leading system message (OpenAI Chat, inserting one if there is none) — for all but `output_shaping_holdout_pct` (default 50, at least 30) of conversations; the only setting that changes what the model is asked, so an experiment, compared per arm at `/v1/stats/analysis/compare` |
| `cold_recompact = "observed"` or `"predicted"` | under `frozen_prefix = "first_seen"`, rewrites the whole conversation on the turn that finds the provider's cache gone, and replays what that turn decided on every turn after it |
| `superseded_reads`, `matured_reads` | with `cold_recompact`: on that turn, also replace a file read a later read contains, and compress as source a read left alone for `matured_quiesce_turns` |
| `reasoning_elide_models = ["…"]` | OpenAI Chat: for the models named, elides the plain-text reasoning of turns before the last user message. Rewrites the finished turn once per user turn |

---

## Not built yet

- **Needle retention**: a measure of whether compression kept what the agent went
on to use. It is the one quality metric that can fail while every ratio improves.
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


`cargo test --workspace` runs **2,236 tests** across ten crates; clippy and rustfmt
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