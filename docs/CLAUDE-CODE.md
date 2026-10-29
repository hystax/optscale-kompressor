# Wrapping Claude Code

`kompressor wrap claude` works, and compresses nothing. Both halves of that
sentence are accurate and neither is a defect: every request reaches the
provider, every answer is correct, the proxy adds no errors and no measurable
latency — and the engine is not permitted to rewrite a single byte.

This document exists so that result is expected rather than discovered, and so
that nobody tries to "fix" it with the one setting that makes it much worse.

Measured against **Claude Code 2.1.278** with a ChatGPT-style OAuth subscription
login (`oauth-2025-04-20`), model `claude-opus-5[1m]`, OptScale Kompressor in
`recoverable` mode.

## Contents

- [What a wrapped session actually does](#what-a-wrapped-session-actually-does)
- [The retrieval tools](#the-retrieval-tools)
- [Why nothing compresses](#why-nothing-compresses)
- [Do not disable prompt caching](#do-not-disable-prompt-caching)
- [Verify it for yourself](#verify-it-for-yourself)
- [Reading the outcome field](#reading-the-outcome-field)
- [What the proxy changes about your request](#what-the-proxy-changes-about-your-request)
- [What compression costs, measured](#what-compression-costs-measured)
- [When wrapping Claude Code is still worth it](#when-wrapping-claude-code-is-still-worth-it)

## What a wrapped session actually does

```bash
kompressor wrap claude
```

That is the whole setup. Claude Code reads `ANTHROPIC_BASE_URL`, which `wrap`
sets, so unlike codex there is nothing to configure. It also gets the retrieval
tools for the length of the session — see [below](#the-retrieval-tools), and
note that no file of yours is edited to arrange it. A measured session:

| | |
|---|---|
| Proxy requests | 7, all `200`, zero errors |
| Family detected | `anthropic_messages`, streamed |
| Answers | correct |
| Tokens in → out | 52,886 → 52,886 |
| **Saved gross** | **0** |

Bodies are forwarded byte for byte. That matters more than it sounds: because
nothing is rewritten, the wrap cannot disturb the provider's prompt cache, and
the cost of running it is the loopback hop. If you compare two wrapped and
unwrapped runs and see different cache figures, that is conversation variance,
not a compression penalty — confirm it by checking that the proxy logged no
`compressed` events at all.

## The retrieval tools

A wrapped session also gets `kompressor`'s tools, as an MCP server Claude Code
launches for itself:

```
mcp__optscale_kompressor__optscale_kompressor_retrieve
mcp__optscale_kompressor__optscale_kompressor_compress
mcp__optscale_kompressor__optscale_kompressor_compress_raw
mcp__optscale_kompressor__optscale_kompressor_stats
```

The one that matters is `optscale_kompressor_retrieve`, and it is there to cover a gap
the proxy cannot. On the proxy, retrieval is in band: the engine injects the
tool, the model calls it, and the proxy answers inside the same round trip —
the client never sees it. That loop only runs while a request is in flight, so
a handle the model meets anywhere else is dead text. A marker in a file it reads
later, in a transcript from yesterday, in output a sub-agent captured: all of
them say a fragment can be fetched, and until now nothing in the session could
fetch it. A tool server is held open for the whole session, so the handle is
resolvable whenever the model meets one.

**Nothing durable is registered.** The server is passed on the command line with
`--mcp-config`, pointing at a file in this session's own run directory, and both
that file and the configuration beside it are deleted when the session ends.
`~/.claude.json` is never opened, and neither is `~/.claude/settings.json`.
There is no `unwrap` step because there is nothing to undo — a session killed
with `SIGKILL` leaves two inert files under `~/.kompressor/run` and no trace
anywhere else.

That is a deliberate choice against the obvious implementation. Writing the
registration into Claude Code's own configuration is what a durable installer
does, and it is how the same feature is built elsewhere; it also means every
way a session can miss its cleanup leaves an entry behind pointing at a binary
that may since have moved. Residue that outlives the thing explaining it is
worse than a feature that has to be asked for each time.

`--strict-mcp-config` is deliberately not passed: it would suppress your own
servers, and this is here to add a tool, not to take yours away.

Two things to know:

* **The store has to be the same one.** The tool server opens the fragment store
  named by its configuration, and a handle minted by the proxy resolves only if
  both name the same file. `refs.path` defaults to the *relative*
  `./data/refs.db`, and the tool server is spawned by the agent, from whatever
  directory the agent is in — so `wrap` resolves the path to an absolute one
  before handing it over. Without that the tool server would open a different
  SQLite file, create it happily, and then report every handle as expired.
* **`--attach` and GUI wiring go without.** Both exist precisely because the
  agent is started by something other than `wrap`, so there is no command line
  to put the flag on. Registering durably so those cases could be covered is the
  trade above, refused.

## Why nothing compresses

Two independent ceilings, either of which alone would be enough.

**One of the two now has a switch.** Everything below describes the shipped
default, `proxy.frozen_prefix = "marker"`. With `first_seen` the first ceiling
lifts and a Claude Code session does compress — 21.6% removed on a measured
Opus run, 56 of 1,363 messages frozen instead of all of them. It is off by
default and there are open issues against it; see "Lifting the first ceiling"
below and §19 of DESIGN.md.

### The trailing cache breakpoint

Claude Code marks its **final** message as a prompt-cache breakpoint. A
breakpoint pins everything before it, so honouring the cache means the whole
conversation is off limits. From a captured request:

```
system blocks: 3     → [1] and [2] carry cache_control ttl=1h
messages: 2          → [1] carries cache_control at block[0]   ← the final message
```

The walker reports that as `frozen_message_count`, and the engine withholds
every message inside it. Across one real session: 5 of 5, 7 of 8, and 10 of 11
messages frozen.

### Lifting the first ceiling

The breakpoint marks Claude Code's *final* message, so the floor it implies —
`max(i) + 1` — covers the messages that arrived on this very turn. Those are not
cached. They are about to be. Measured across a 20-turn run,
`frozen + opaque == messages_total` on **every single turn**: the engine was
handed nothing it was allowed to move, while the conversation tail went into the
cache untouched.

`proxy.frozen_prefix = "first_seen"` freezes only as far as the provider's cache
reached when the proxy first saw the conversation, so the tail — in no cache
entry yet — becomes compressible without invalidating anything. The prize grows
with session length, because a frozen tail token is re-read on every later turn:

| | 20-turn Sonnet | 70-turn Opus |
|---|---|---|
| tail tokens | 19,431 | 71,519 |
| lifecycle cost | $0.1417 of $0.4202 | $1.9721 of $3.6355 |
| share of bill | 34% | **54%** |

At 70 turns a tail token averages $27.58/MTok against $5.00 for plain input; the
earliest costs $40.25/MTok, eight times face value.

What it does not yet have is a paired control, and one measured run showed cache
churn on the turn the first handle appears. Both are recorded as open issues in
§19 of DESIGN.md. Default stays `marker`.

### Two side effects of the custom base URL

Neither is OptScale Kompressor's doing, and both are worth knowing before you wrap a
session:

* **`/remote-control` disappears.** Claude Code v2.1.196 and newer disable it
  whenever `ANTHROPIC_BASE_URL` points anywhere other than `api.anthropic.com`.
  The command simply stops existing; nothing explains why.
* **Tool schemas stop being deferred.** With a custom base URL and
  `ENABLE_TOOL_SEARCH` unset, Claude Code materialises every MCP and system
  tool schema into its context window instead of leaving them behind the
  server-side tool search — which breaks sub-agents and forces compaction. For a
  proxy whose whole purpose is a smaller prompt this is the worst possible
  side effect, so `wrap claude` sets `ENABLE_TOOL_SEARCH=true` when you have not
  set it yourself. Both were found and reported upstream by headroom (GH #746,
  GH #1779).

**The floor is now per block, which is what that `at block[0]` was telling us
all along.** Prompt caching pins the prefix up to and including the marked
*block*; the blocks after it — which is where the tool output lives — are in
nobody's cache and can be rewritten without costing a cache read. The walker
lowers `frozen_message_count` to stop before a message whose breakpoint has
content after it, and protects that message's blocks up to the breakpoint
individually instead. A message wholly inside the prefix is reported exactly as
it always was, so nothing downstream sees a change where the behaviour did not
change.

This is the right behaviour. Rewriting a byte inside the pinned region
invalidates the provider's cached prefix for the whole conversation, and on this
workload a cache read is roughly a tenth the price of a fresh input token — so
compression would have to remove about 90% of the prompt just to break even on
the cache it destroyed.

It is the right behaviour *while the cache is alive*. Once the prefix has lapsed
past the provider's TTL there is no cached read left to invalidate, and the
region the freeze was protecting becomes the cheapest text in the request to
remove. A caller that can tell — the gap since the last turn exceeded the TTL,
or the previous response reported zero cache-read tokens — says so with
`RequestOptions::prefix_cache = PrefixCache::Cold` and the freeze lifts for that
request (§5.12b in DESIGN.md). Nothing in this proxy sets it yet; the default is
`Warm`, which is what a caller that cannot tell should keep.

### Most of the request is not `messages`

The walkers walk `messages`. They do not walk `tools`. For a request whose user
turn is the two words `say OK`:

```
total body        141,064 bytes
  tools           116,598  (82.7%)  <- never walked
  system            9,929   (7.0%)  <- protected role, and cache_control on top
  messages         16,659  (11.8%)  <- all the engine can see
```

Twenty-eight tool schemas are 83% of the payload. Even with every protection
lifted, the addressable surface of a Claude Code request is around a tenth of
what is billed. Measured on the largest request of a real session with all
protections removed:

```
engine saw 8,163 of 109,586 billed prompt tokens (7.4%); saved 3,008 (2.7% of billed)
```

**And walking them would not help.** A second capture, taken against a fake
upstream so it cost nothing — 25 tools, 115,063 bytes, `tools` 64.5%, `system`
23.9%, `messages` 10.7% — was fed through the engine directly:

| | lossless | aggressive |
|---|---|---|
| system prompt | 0.4% | 70.1% |
| tool descriptions | 0.7% | 81.9% |
| tool schemas (wire-serialized) | 1.0% | 7.1% |

72% of the tool bytes are natural-language `description`, which is exactly what
lossless transforms cannot touch. The whole lossless opportunity across `system`
and `tools` is **159 tokens of 23,622 — 0.7%**, worth $0.0018 against a $0.6588
session. The aggressive column emits no handles, so its 70–82% is the agent's
instructions and its knowledge of its own tools *discarded*.

Two measurement traps to avoid repeating: `recoverable` mode reports 70% on the
system prompt, but that is one handle hiding 19,334 bytes — the model would have
to call a tool to read its own instructions — and schemas first measured 16.6%
lossless only because the test file had been pretty-printed with `indent=1`.
Against the bytes actually sent it is 1.0%.

The walker not reading `system` and `tools` is the right call, not a gap.

### The largest tool results never arrive

The remaining tenth is smaller than it looks, because Claude Code truncates its
own big tool outputs before they are ever sent. Running `cat` on a 66.6 KB file
inside a wrapped session produced this in the next request:

```
<persisted-output>  output too large (66.6KB) — saved to a file, 2KB preview retained
```

The proxy saw the 2 KB preview. The 66.6 KB never left the client, so there was
nothing to compress and no handle to mint — confirmed by the engine reporting no
elisions for that turn at all.

This is worth stating plainly because it inverts the usual intuition: the
payloads a compression proxy exists for are exactly the ones this client
disposes of first. What reaches the engine is the traffic already small enough
that Claude Code did not consider it worth persisting.

## Do not disable prompt caching

`DISABLE_PROMPT_CACHING=1` removes the breakpoints, and compression does start
working. It is still the wrong trade, by a wide margin, and this was measured
rather than reasoned:

| | caching on (default) | caching off |
|---|---|---|
| Messages compressed | 0 of 24 | 7 of 30 |
| Engine tokens | 52,886 → 52,886 | 32,027 → 22,888 (**−9,139**) |
| Handles minted | 0 | 3 |
| Billed prompt tokens | ~174K cache reads + ~89K cache writes | **524,922 uncached** |
| Full-price equivalent | ~128K units | ~525K units |
| Wall clock | 28 s | 54 s |

Disabling the cache to unlock 9,139 tokens of compression costs roughly **four
times** as much and doubles latency. The engine's saving is about 1.7% of the
uncached prompt volume it created.

There is no configuration of OptScale Kompressor that makes this trade work on Claude
Code. The fix, if one ever exists, is client-side: a breakpoint placed before
the tool-result history rather than after it.

## Verify it for yourself

A wrapped session tells you as it exits, and on Claude Code the expected
reading is the middle one — traffic arrived, and none of it was eligible:

```console
kompressor: 2 request(s), nothing compressed — every message was withheld.
           Usually a client that pins its whole prompt with cache_control; see docs/CLAUDE-CODE.md.
```

That is success. `no requests reached the proxy` would be the failure, and it
is not what Claude Code does — it honours `ANTHROPIC_BASE_URL` correctly.

For the detail behind that line, run a task and then:

```bash
kompressor stats summary
```

A healthy wrapped Claude Code session looks like this, and the zero is the
expected reading:

```
requests            3
messages            24 total, 0 compressed
tokens              52886 in, 52886 out
saved gross         0
```

To confirm the cause rather than assume it:

```bash
sqlite3 ./data/stats.db \
  "select messages_total, messages_skipped, protected_by_reason, outcome
     from optimization_events order by time_ms;"
```

`protected_by_reason` reading `{"frozen": N}` with `N` close to
`messages_total` is the cache breakpoint. Anything else — `role:assistant`,
`flagged` — is a different story and worth looking at.

## Reading the outcome field

`outcome` under-reports the frozen case, so do not take it at face value here.
[`CompressStats::outcome`](../crates/kompressor-engine/src/messages.rs) returns
`prefix_frozen` only when `frozen_messages == skipped_messages` exactly. One
eligible-but-unclaimed message flips the label:

| request | frozen | skipped | reported |
|---|---|---|---|
| 1 | 5 | 5 | `prefix_frozen` |
| 2 | 7 | 8 | `no_compressible_content` |
| 3 | 10 | 11 | `no_compressible_content` |

The operator-facing warning (`the client pinned the whole prompt with
cache_control`) is gated on the same condition, so on that session it fired once
out of three. Read `protected_by_reason` instead; it is always accurate.

## What the proxy changes about your request

A subscription CLI is recognised by the provider it talks to, so the proxy tries
not to be the reason a request stops looking like that client.
[`authmode::classify`](../crates/kompressor-proxy/src/authmode.rs) reads the
User-Agent and the credential together: a known CLI carrying something that is
*not* a per-token API key is classified `subscription`, and on that path the
proxy does **not** add its `x-request-id` to the upstream request. The id still
reaches every stats row and log line; only the wire is left alone. A request
from the same binary holding a plain API key is `payg` and keeps the header,
because there is no subscription at stake and the correlation id is worth more.

Verified against Claude Code 2.1.278:

| caller | classified | `x-request-id` upstream |
|---|---|---|
| `claude-cli/2.1.278` + `Bearer sk-ant-oat…` | `subscription` | absent |
| `claude-cli/2.1.278` + `x-api-key` | `payg` | present |

**One difference remains and is not yet fixed.** Claude Code sends
`accept-encoding: gzip, deflate, br, zstd`; the proxy replaces that with
`identity`, because a compressed response is bytes the retrieve loop cannot read
(see [`headers`](../crates/kompressor-proxy/src/headers.rs)). Closing it means
teaching the client to decompress and then stripping `content-encoding` from
what is relayed onward, which is a real change rather than a header tweak.

## What compression costs, measured

Measured 2026-10-25 on two 89-task Harbor runs of Claude Code on Claude Opus
5.5 with one binary: `proxy.frozen_prefix = "first_seen"` against compression
off (`proxy.compress = false`). Both solved 79 of 89. Scored task by task on the
proxy's own bill with `scripts/harbor/panel-score.py`, which prices every
upstream request at its model's rates — Harbor's own cost missed requests
outside the agent's trajectory and moved the comparison by nine points on its
own:

| Compressed vs off | Change | 95% CI over tasks |
|---|---|---|
| Cost per agent step | +0.9% | −3.6% .. +5.8% |
| Cost per trial | +9.0% | +0.4% .. +18.8% |
| Cost per solved task | +7.9% | −11.4% .. +29.1% |

Three facts account for it:

- **Re-sent history is already cheap.** About 93% of the billed prompt was cache
  reads, and Claude Opus 5.5 reads at $0.20 against $4.00 input. Compression
  saved about 8% of input on an ordinary request, roughly $1.70 over the run.
- **A retrieval round is an extra answer.** The model writes the retrieve call
  and answers again after the hidden round, so a request with one cost about
  $0.046 more than an ordinary one, mostly output: about $3.30 over 70 such
  requests. Elided code bodies were fetched back 57% of the time, other elided
  text 14%.
- **Most lost cache is expiry, not rewriting.** 92% of the cache-read drops
  followed a pause past Anthropic's five-minute entry lifetime, while the agent
  waited on a build or a test run.

Two settings act on those. Measured together on the same 89 tasks, they came to
**+0.2% per step** against compression off (95% CI −4.6% .. +5.6%), with 6% more
agent steps. That run found two faults, both fixed since: plain text folded
similar lines from anywhere in a payload into one, and tasks where that
happened took 17.5% more steps; and the adaptive TTL switched one request late,
so every switching conversation wrote its prefix twice. The two settings:
`engine.enable_code_aware = false` stops eliding function bodies (removal on
the replay corpus falls from 48.8% to 43.3%, and the fetches that cost more
than they saved go with it), and `proxy.cache_ttl = "adaptive"` asks for
one-hour cache entries in a conversation once it has paused past five minutes,
which is estimated at 5.6% off the compressed run. Both apply with compression off as
well (the proxy follows the conversation in either mode); `"1h"` costs more than
it saves on runs without long pauses.

A third setting acts on the fetches that remain. `engine.min_elision_saving_chars
= 4000` ships a tool result whole when compressing it would leave a handle but
save fewer than 4,000 characters. Re-sent at cache-read prices, such a result
saves less than the one fetch it invites, and it invites one often: across ten
recoverable runs, tool results saving under 4,000 characters were fetched back
30% of the time. Replaying all ten runs' tool outputs through the current
engine, the gate removes 39% of the fetch rounds that would still happen (167
to 102) and gives up a third of an already small removal value ($2.13 to $1.36
over the ten runs), about $1.20 ahead at $0.03 a round and still ahead at $0.02.
Markers without a handle are not gated.

The packaged configurations (`/etc/kompressor/config.toml` on Linux,
`~/.config/kompressor/config.toml` on macOS) ship these four settings, and
`retrieve_tool = "always"`, under `[proxy.providers.anthropic]`, with Codex on
the deployment defaults. An
install that already has a config keeps it: dpkg leaves an edited conffile in
place beside a `.dpkg-dist` copy, and the macOS installer never overwrites one.
To set them by hand for Claude traffic only, leaving any OpenAI clients of the
same proxy on the defaults, scope them to the provider (DESIGN §12.11):

```bash
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__ENABLE_CODE_AWARE=false
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__MIN_ELISION_SAVING_CHARS=4000
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_TTL=adaptive
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__FROZEN_PREFIX=first_seen
```

A fifth setting, **off by default and not in the packaged configurations**,
keeps the cache entry alive through those pauses instead of paying for them.
With `proxy.cache_keep_warm = true`, after a response that stopped on
`tool_use` the proxy re-sends that request with `max_tokens: 1` every 270
seconds until Claude Code's next request arrives, at most 11 times. Each ping
costs the conversation at the cache-read price plus one output token, and is
recorded as its own `proxy_requests` row with `keep_warm = 1`. Simulated over
the request logs of seven 89-task runs, it would have saved $1.84–$3.00 a run
(5–9%) after $0.34–$0.58 of pings, which is more than compression saves on the
same runs. Measured live on one 89-task run, it cut cache-write spend per agent
step by 20% and cache-loss turns from 13 to 2, for 25 pings costing $0.15. It
is off because it is the one thing the proxy does that no client asked for,
using the client's credentials while the client is busy elsewhere (DESIGN
§12.10):

```bash
export KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_KEEP_WARM=true
```

## When wrapping Claude Code is still worth it

Compression is not the only thing the proxy does, and the rest of it costs
nothing here:

- **Statistics.** `proxy_requests` records every request, model, status, latency
  and the full usage breakdown — prompt, completion, cache read and cache write
  — per turn. That is a usable picture of where a subscription is going, and it
  exists whether or not anything compressed.
- **A correctness baseline.** A wrapped session that forwards byte-identically
  is a control run. If a change to the engine ever starts rewriting Claude Code
  traffic, the diff against these numbers is the alarm.
- **It is free.** No rewrites, no cache disturbance, one loopback hop.

For actual compression on an agent workload, `codex` is the supported path —
see [CODEX.md](CODEX.md). Seven measured sessions on a ChatGPT subscription
removed **60.1% overall**, ranging from 78.3% on grep output to 16.4% on build
output; what a session saves is decided by what it reads. The difference
between codex and Claude Code is entirely client-side prompt structure, not
engine quality.
