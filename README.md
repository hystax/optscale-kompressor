# OptScale Kompressor

![CI](https://github.com/hystax/optscale-kompressor/actions/workflows/ci.yml/badge.svg)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![MSRV 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)

**Deterministic prompt compression for coding agents, in one Rust binary.**

Every step a coding agent takes, it re-sends its entire working history to the
model: every apt-get log, every compiler warning, every progress bar. On a long
task the model re-reads the same build output dozens of times, and you pay for
every read.

We built OptScale Kompressor to stop that, for many teams and agents at once.
The same input compresses to the same bytes every turn, so the prompt cache
keeps working, and every removed detail stays one retrieval away.

On Terminal-Bench 2.1 it takes 22.7% off the real bill. How is it different from
Headroom, the best-known open-source context compressor, which has grown into a
platform? Kompressor is deliberately just a compressor, and it saves about twice
as much.

One rule decides everything else: **the same input compresses to the same bytes,
every turn.** On agent traffic most input tokens are billed as prompt-cache reads,
at roughly a tenth of the fresh price — measured at 93.0% of billed prompt on an
89-task run. A compressor whose output shifts between turns breaks that cache and
costs more than it saves.

```bash
kompressor wrap codex
```

That starts the proxy, points Codex at it, runs Codex, and cleans up on exit.
When the session ends it tells you what happened:

```console
kompressor: 26 request(s), 126 message(s) compressed, ~2029706 tokens saved (2015180 net of retrieval)
```

---

## Results

Every figure below is the difference between real provider bills, with the
prompt-cache discount included. Self-reported savings, which price removed tokens
at the uncached list rate, are not used. The full write-up, with the task-by-task
appendix, is
[Twice the Savings of Headroom on Coding-Agent Traffic](https://optscale.ai/blog/context-compression-coding-agents).

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

---

## Install

Pre-built binaries for Linux (x86_64, arm64) and macOS (arm64, x86_64), with
checksums, are on the [Releases page](https://github.com/hystax/optscale-kompressor/releases/latest).

With Cargo (Rust 1.90 or newer):

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

`make deb`, `make rpm`, `make static` (musl), `make docker` and
`make macos-pkg-docker` build packages; each says what it needs if a tool is
missing. [docs/QUICKSTART.md](docs/QUICKSTART.md) covers running it as a service,
and [docs/MACOS.md](docs/MACOS.md) covers the macOS paths.

---

## Quick start

### Codex — the supported path

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

### Claude Code — wrapped, and deliberately not compressed

`kompressor wrap claude` runs and forwards Claude Code's requests untouched. That
is not a gap waiting to be filled: Claude Code marks its final message as a
prompt-cache breakpoint, which pins the whole conversation, and rewriting any byte
inside it would invalidate a cache billed at about a tenth of fresh input.

What the wrap does give it is retrieval. `wrap claude` hands Claude Code the
retrieve tools as an MCP server it launches itself, so a `ref:k_…` handle resolves
anywhere in the session rather than only while a request is passing through the
proxy. Nothing durable is written — no `~/.claude.json`, no `~/.claude/settings.json`
— so there is no `unwrap` step.

That paragraph describes the **default**, `proxy.frozen_prefix = "marker"`.
Setting `first_seen` instead compresses only what the provider has not cached
yet, and on Anthropic traffic it is the difference between nothing and
**43.6% removed** over a sixteen-payload session — because Claude Code's
trailing breakpoint pins the whole conversation under the marker rule, so there
is nothing left for the engine to touch.

It is no longer experimental. Safety is established: excess writes measured 0 on
10 of 13 runs, every non-zero figure predating the `was_warm` and discard-guard
fixes, and a 40-turn Opus run held the incremental-cache identity 37 times out of
37 while removing 33.0% at 96.6% cache reads. It is what the packaged default
sets for Anthropic clients, and what `docs/CLAUDE-DESKTOP.md` requires.

The engine default stays `marker`, for one remaining reason and it is not churn:
the **cost** case is unmeasured, and the obvious experiment is invalid —
`$/MTok out` correlates +0.80 with run length, so two runs of different
trajectories cannot be compared however carefully they are matched. That wants a
replay-based A/B over recorded turns. Until then the default is conservative on
economics rather than on safety. The argument in full:
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
mount or handler changes to support it. Point Desktop at it with `HTTPS_PROXY`
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
[docs/CLAUDE-DESKTOP.md](docs/CLAUDE-DESKTOP.md).

### Any client, through the proxy

```bash
kompressor serve                                    # every surface on 127.0.0.1:8817
export OPENAI_BASE_URL=http://127.0.0.1:8817/v1     # OpenAI Chat Completions or Responses
export ANTHROPIC_BASE_URL=http://127.0.0.1:8817     # Anthropic Messages: the root, no /v1
```

### One file, with an explanation

```bash
kompressor compress build.log --explain
```

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
| MCP server | `kompressor serve --mcp` (streamable HTTP): `optscale_kompressor_retrieve`, `optscale_kompressor_compress`, `optscale_kompressor_compress_raw`, `optscale_kompressor_stats` |
| CLI        | `kompressor compress`, `retrieve`, `detect`, `stats`, `bench`, `check-config`                                                                                               |
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

---

## Not built yet

- **Claude Code compression** — see [Claude Code](#claude-code--wrapped-and-deliberately-not-compressed)
for why the default is to forward untouched.
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


`cargo test --workspace` runs **1,157 tests** across ten crates; clippy and rustfmt
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