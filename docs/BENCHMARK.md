# Benchmarking against a live agent

`make bench` replays a corpus: it answers *what does the engine do to these
payloads*. This answers a different question — *what happens when a real coding
agent works a real task through the proxy for twenty minutes*. They fail in
different ways, and the second one has found things the first could not.

The two are connected by `kompressor bench extract`, which turns the runs this
document produces back into a corpus the first one replays:

```bash
kompressor bench extract --from runs/ --out fixtures/corpus.jsonl.gz \
  --exclude 'resolved route /api/v'
```

It reads each run's **ref store** — the fragments the engine actually elided —
so what it yields is a sample of CCR deferrals, not of all traffic: a lossless
reduction mints no handle and so never appears. Two filters earn their keep.
`--exclude` keeps this harness's own self-test payload out, which was otherwise
the single largest fragment in one run; `--max-control-ratio` (default 0.01)
drops binary an agent happened to `cat`, which on 476 real fragments separated
absolutely — two payloads at 0.28 and 0.51, every other one at exactly 0.0.

```
codex (task container) --http--> kompressor serve --proxy --api --https--> provider
```

```bash
export UPSTREAM=https://api.openai.com
export OPENAI_API_KEY=sk-...
make harbor-gates          # every gate, no spend -- run this first
make harbor-smoke          # 1 task, ~$2, ~10 min
make harbor-suite          # 10 tasks
scripts/harbor/suite.sh full   # every task in the dataset (~89)
```

Arms, each into its own run directory so they can never be mixed:

```bash
MODE=recoverable  scripts/harbor/suite.sh smoke   # handles + server-side retrieve (default)
MODE=aggressive   scripts/harbor/suite.sh smoke   # elide, retain nothing
MODE=off          scripts/harbor/suite.sh smoke   # passthrough over the same network path
NO_RETRIEVE=1     scripts/harbor/suite.sh smoke   # retain, but never offer the tool
```

`MODE=off` rather than "run without the proxy" is deliberate: a baseline taken
without the proxy also removes a network hop, and then the comparison measures
two things at once.

It reaches the proxy as `[proxy] compress = false`, not as an engine mode. The
engine has three modes and none of them is a passthrough, so `[engine] mode =
"off"` fails to parse — which is what the control arm did, every time, from the
first run to 2026-10-07: `--mode off` was an argparse choice the config writer
passed through verbatim into a field that cannot hold it, and the run died at
`check-config` before spending anything. Thirty-odd compressed runs were
collected in that window and not one has a baseline to be read against. The
emitter and the self-test now share one function, so the arm cannot be
described in two places and drift.

**`compress`, not `enabled`.** They are not two names for one switch.
`proxy.enabled = false` means the proxy surface is not served: `serve` never
merges the proxy router, so every proxied path answers 404 and the run dies at
the through-proxy preflight. It also takes the network hop with it, which is the
one thing `MODE=off` exists to keep. `compress = false` leaves the router
mounted and the bodies parsed, so usage and cache reads are recorded exactly as
on a compressed run and the arms differ in compression alone. The distinction
cost a run to find: the first attempt at this control arm used `enabled` on the
strength of a comment in `config.rs` calling it "the kill switch: off means
forward unchanged", which describes `handler.rs` — reachable only when an
embedder mounts the router itself, and not what `serve` does.

The switch is also what the run reports. `compress = false` resolves to
`ProxyMode::Off`, so the response says `x-kompressor-mode: off` — previously a
non-compressing deployment answered `recoverable`, because the default resolved
through `ProxyMode::On(engine.mode)` with no way to express off. A control arm
that cannot be told from a compressed one in its own telemetry is not a control.
A request header cannot turn compression back on either.

### The first paired result

2026-10-07, `smoke` (one task, one attempt), `gpt-5.5`, same upstream, 37
minutes apart. The first time both arms have run, because until that day the
control arm could not start.

| | compressed | control | |
|---|---|---|---|
| resolved | 1/1 | 1/1 | |
| cost | **$2.0908** | **$2.2234** | **$0.1326 saved, 5.96%** |
| prompt in | 2,160,089 | 2,536,204 | 14.8% fewer |
| of which cached | 2,064,128 | 2,415,488 | 14.5% fewer |
| fresh | 95,961 | 120,716 | 20.5% fewer |
| output | 19,298 | 13,735 | **40.5% more** |
| proxy requests | 47 | 39 | 8 more turns |
| prompt per request | 46,332 mean / 44,657 median | 65,031 / 76,064 | 29% / 41% lower |

The engine reported `removed 1,520,414 tokens (56.1%)` on that run. The billed
prompt difference between the arms is 376,115 tokens — **the engine's number is
4.0x the measured one**, for the two reasons §12.5b gives: `removed` is a
per-turn sum, and what it removes is 93% provider-cached text billed at a tenth
of the fresh rate. The prompt side did save $0.2995. The output side gave
$0.1669 of it back, because the compressed run took 8 more turns and wrote more
per turn. Net 5.96%.

**That 5.96% is not an effect size, and the next run proved it.** A second
control arm, same task, same arm, 11 minutes later, cost **$1.5151** against the
first control's $2.2234 — a spread of $0.7083, 46.7%, on 904,020 fewer prompt
tokens. The control-to-control spread is **5.3x the compression saving it was
supposed to measure**, and the compressed run is *more* expensive than the
second control. Pick a control and compression saves 6% or costs 38%.

| | prompt in | requests | cost |
|---|---|---|---|
| compressed 18:49 | 2,160,089 | 47 | $2.0908 |
| control 19:26 | 2,536,204 | 39 | $2.2234 |
| control 19:37 | 1,632,184 | 37 | $1.5151 |

The variance is the agent's trajectory, not the proxy. One task run three times
takes a different path each time, and the path dominates everything else.

**What this design can and cannot measure.** The first-order saving — fewer
prompt tokens for the same conversation prefix — is measurable precisely and
without a control at all, from `tokens_before`/`tokens_after` and the body
capture. The second-order cost — more turns, more output, retrieval round trips
— is real, is what decides whether CCR pays, and is buried under trajectory
noise at this sample size. Treating one A/B pair as the answer gets the sign
wrong as easily as the magnitude.

### The corpus tracks what real runs decline, not only what they compress

`kompressor bench extract` builds a corpus from `kompressor-refs.db` — the
fragments a run *elided*. That is the right source for measuring how well the
compressors do on what they claim, and it can never produce a payload they
declined, because a declined payload is never stored. So the corpus drifted
toward the content the engine is already good at.

Measured on the 89-task run of 2026-10-12: `declined:text` is the largest
addressable skip bucket at **8,651 messages / 3,629,012 tokens**, behind only
`protected:role:developer` (protected by design) and `below_min_chars`
(correctly left alone). Replaying every distinct tool output from that run's
captured bodies puts 391 eligible payloads in `untouched:text`, 4.7% of the
replayed chars — against 1.1% in the committed corpus, a 4x under-weighting of
the one bucket most worth attacking.

62 of them were added, lifting the bucket to 78 payloads and **4.8%** of corpus
chars, within a tenth of a point of the run's own share. Selection is
stratified by size and takes the most-carried payload in each stratum first —
`requests` median 53, max 299 — so the sample spans 515 to 9,818 chars rather
than being a handful of large files, and weights what the bill actually sees.
It is deterministic, so re-running the extraction reproduces the corpus.

**Only payloads at or above `engine.min_compress_chars` (512) are worth
adding.** Eligibility is what the replay scores by: a shorter payload is
counted by neither `coverage_pct` nor the bucket totals. A first sample ignored
this, took 129 payloads and had 41 of them count — the file grew and no number
moved.

The recorded baseline moves with the corpus and that is the point: coverage
69.63% -> 67.04%, billed coverage 63.67% -> 59.47%. **Neither is a regression.**
No payload compresses differently; the corpus now contains more of the material
the engine declines, so the average over it is lower and more honest. A bucket
comparison is what the gate actually checks, and every bucket is unchanged to
four decimal places.

### Claude Code payloads, and what they did to the averages (2026-10-25)

The corpus held only Codex traffic, so none of Claude Code's tool outputs —
`Bash` and `Read` results, often a listing glued to a `cat` — was measured at
all. `scripts/bench/extract-tool-outputs.py` reads a run's captured *ingress*
bodies, so a payload the engine declined is extracted like one it compressed,
and deduplicates every `tool_result` of at least 512 characters across the runs
given. From six Anthropic runs that added **535 payloads** (359 → 894). Captured
agent output is not safe to commit as it stands, so the script drops anything
credential-shaped (a secret-scrubbing task plants fake keys, which still trip
push protection) and anything carrying an email address outside a short
placeholder allow-list (git authors, a CV); 5 and 6 payloads respectively. Run
today it reproduces 531 of the 535 — it is stricter than the first pass, which
kept four hex dumps whose ASCII column happens to read like an address.

The recorded baseline moved with it, and as before **neither move is a
regression**: coverage 66.99% → 58.76% on the same engine, because Claude Code
traffic carries more short prose and more source the agent is editing. The
e2e floors moved for the same reason (`e2e/runner/floors.json` records the
measurement); two were lowered, which that file otherwise never does, because a
floor is a fraction of a corpus and this is a different corpus.

It moved again on 2026-10-25, this time because the engine changed what it
removes: coverage **58.92% → 48.78%** on the same 894 payloads. Plain text now
folds only within a run of consecutive lines, and disassembly ships whole
(DESIGN §7.10c). Most of what the wider fold removed was source the detector
does not name and data whose order is the content. On the 89-task tuned run,
the tasks where that fold fired took 17.5% more agent steps than the
uncompressed run. That is a cost per step the coverage figure never showed.

### Score a paired comparison on the proxy's bill

Two scripts read finished runs without spending anything:

- `scripts/harbor/panel-score.py --arm off='<glob>' --arm rec='<glob>'` scores
  arms task by task: cost per agent step, per trial and per solved task, each
  with a 95% bootstrap interval over tasks. Costs are the **proxy's bill**, every
  upstream request priced at its model's rates and assigned to its task by the
  trial's tool-call ids and its first user message. Harbor's own cost is printed
  and not used: it sees only the requests in the agent's trajectory, and on two
  89-task runs it missed $5.11 of one arm's bill and $2.66 of the other's.
- `scripts/harbor/ttl-check.py <run-dir>...` reads each step's
  `cache_creation` split from the trajectories — five-minute against one-hour
  writes, which the proxy's stats do not keep — and reports how much cache a
  request kept after a pause, and whether a conversation's switch to one-hour
  entries read the five-minute entry before it (§12.10 of DESIGN.md).

Per-task log cost per step varied with an sd of 0.27–0.37 between two runs of
the same configuration, so a panel of sixteen tasks with four repeats per arm
resolves an effect of about 7%.

### Three bases for one number, and only one of them compares

`compressible X -> Y` sums what the engine looked at on **every request**. An
agent re-sends its whole history each turn, so a byte that survives 300 turns is
counted 300 times. That is a true statement about the engine's work and a
useless one about size, and it has been misread as a corpus size: the 89-task
run of 2026-10-12 printed **415,987,844**, of which 79% was one 302-turn
conversation whose content reached 1.59M tokens and was counted 207 times.

The report now prints three lines:

```
compressible     415,987,844 tokens -> 49,368,464        88.13% removed
billed-weighted   47,332,477 tokens ->  7,053,166        85.10% removed
first send only    6,370,770 tokens ->  2,351,466        63.09% removed
```

* **flat** — every request, every time. What the engine did.
* **billed-weighted** — a re-send priced at `CACHE_READ_RATE` (0.1), because
  that is what it costs: fresh input once, a cache read on every later turn,
  and a cache read is a tenth of the input rate on both providers measured here.
  **This is the one to compare runs on.**
* **first send only** — the content itself, each byte once.

This project already had the treatment and had not applied it here.
`kompressor-bench` carries the same `CACHE_READ_RATE = 0.1` and the same
`1.0 + (requests - 1) * CACHE_READ_RATE` (`replay.rs`), and the shipped corpus
records `requests` per payload — median 28, max 261, so a median payload weighs
3.7 first-sends and the heaviest 27. The corpus bench has been pricing re-sends
correctly all along; the Harbor reporter was the one place still counting them
flat, which is why its number was the one that got misread.

The constant and the reasoning come from OptScale Kompressor's
`bench/metrics.py`, which met this first: *"one carried 261 times costs 27
first-sends, not 261"*, and measured its own coverage at 48.92% per distinct
payload against 56.29% per billed char. Headroom has no equivalent — its
`tokens_saved = tokens_before - tokens_after` accumulates flat — so a headroom
figure quoted against this one must be compared against the **flat** line, not
the weighted one.

Two details are load-bearing. The weighting is applied **per session, not per
conversation**: a conversation fingerprint can cover two agent sessions (21 of
88 on that run), and the shortcut `unique + (flat - unique) x rate` is exact
only within one monotonic session — applied across a break it prices the second
session's content as a re-send of the first. And the flat line stays, because
the two answer different questions and dropping it would make a number that has
been quoted for months silently change meaning.

Note what moves and what does not. The **percentage** is robust — 88.13% against
85.10% on the 89-task run, and 58.73% against 58.87% on set11, where no
conversation ran long enough to matter. It is the absolute context figure that
was misleading, by 8.8x and 6.4x respectively.

### Decompose the bill before blaming compression

A run's total is three terms at the measured prices, and they reproduce each
stated total exactly. The 89-task runs, against OptScale Kompressor and
headroom on the same suite:

| | cache reads | uncached | output | total |
|---|---|---|---|---|
| OptScale Kompressor 09-13 | $32.39 | $19.27 | $25.88 | **$77.54** |
| headroom 09-08 | $39.81 | $24.22 | $28.22 | $92.25 |
| kompressor-rs 10-12 | $44.74 | $24.18 | $29.83 | **$98.75** |

**Cache reads are 45% of this project's bill**, and they scale with turns times
prefix: that run took 21% more turns with a 14% larger prefix on each, which is
38% more cache-read tokens and $12.35 of the $21 gap. The cheapest run is the
one that finished in fewer turns, not the one that compressed best — it removes
32% on the matched subset where this one removes 54%, and it is cheaper anyway.

Two hypotheses that looked obvious and were wrong. **Cache loss is not the
differentiator**: computed identically from each run's own rollouts it is 9.7%,
11.8% and 14.9% for the OptScale runs, 9.7%, 11.4% and 9.6% for headroom's, and
13.7% here — mid-range, and worse in one of the comparisons. **The retrieve
loop is not recoverable either**: it is real at 1.8x prompt on the rows that run
one, and §20 item 12 records the measurement showing it cannot be pre-empted.

So a cost comparison that reads a compression percentage and stops has read the
wrong column. Removal rate and bill are only loosely coupled; what the provider
charges for is tokens it has not cached, repeated once per turn.

### Compare per request, not per run

Four runs, two per arm (2026-10-07):

| arm | prompt in | requests | tokens/request | cost |
|---|---|---|---|---|
| compressed | 2,160,089 | 47 | 45,959 | $2.0908 |
| compressed | 1,394,050 | 41 | 34,001 | $1.5047 |
| control | 2,536,204 | 39 | 65,031 | $2.2234 |
| control | 1,632,184 | 37 | 44,113 | $1.5151 |

The dominant noise term is how many turns the agent took, and it is not small:
the two controls differ by 46.7% on cost. Dividing it out changes what the
experiment costs by two orders of magnitude.

| metric | apparent effect | pooled sd | runs/arm at 80% power |
|---|---|---|---|
| cost per run | +3.8% | $0.458 | **~655** (~$2,400, ~131h) |
| cost per request | +17.1% | $0.0084 | **~16** (~$58, ~3h) |
| prompt tokens per request | +26.7% | 11,623 | **~10** (~$37, ~2h) |

So the report prints billed prompt tokens per request beside the request count.

**Per request is half the comparison.** Dividing by the request count also
divides out a cost that is genuinely compression's: all four runs put the
compressed arm above the control on turns taken (47 and 41 against 39 and 37).
If that holds up it offsets much of the per-request gain, and a benchmark that
only reports the normalized figure would never show it. Report both, and
multiply them to get back to what was actually spent.

**An n of two per arm does not support a conclusion**, and the pairing is where
that bites. Pairing the runs by magnitude gives −14.8% and −14.6% in prompt
tokens, which looks like a stable effect; the other pairing of the same four
runs gives −32.3% and +45.0%. The pairing is arbitrary, so the consistency is an
artifact of choosing it. The power column above is the honest reading: on cost
per run this stage cannot answer the question at any price worth paying, and on
cost per request the experiment becomes affordable — which is the finding.

What the pair does establish is the shape of the reporting error: a 56%
reduction of what the engine looked at came with a 14.8% reduction in billed
prompt tokens, and anyone reading `removed` as a saving is reading it wrong by
about 4x. That conclusion does not depend on which control is picked — it
compares the engine's own counter against the same run's billed total.

The measured price table (§12.5b) predicted both arms' billed cost to **0.001%**
from their token counts, on data recorded after it was fitted.

The control arm names conversations the same way the compressed arm does, from
the conversation's opening, and records each row's model, so the arms can be
compared conversation by conversation. Runs recorded before 1.4.0 have
`conversation = NULL` and no model on every passthrough row; `panel-score.py`
assigns those by the trial's tool-call ids and first user message instead.

## The gates

Each one exists because a run can otherwise complete, report a number, and mean
nothing. They all run before Harbor starts.

| | |
|---|---|
| **Effective configuration** | generated into the dump, read back with `kompressor check-config`, and compared against what was asked for |
| **`/healthz` + `/readyz`** | per-subsystem: a proxy listening with a dead ref store mints no handles and still looks up |
| **Compression self-test** | `/v1/compress/raw` over ripgrep output and a distinct-line log; asserts bytes fell **and**, in `recoverable`, that a handle was minted |
| **Upstream preflight** | one tiny `/v1/responses` at the provider: the key and the model name are real |
| **Through-proxy preflight** | the same call from the address the agent will use — the only check that exercises routing, auth passthrough and the mount together |

### Why the upstream preflight checks compression

Before `api_family` defaulted to `"auto"`, it defaulted to `"passthrough"`,
which meant *do not parse this body*:

```rust
let family = if upstream.api_family.eq_ignore_ascii_case("passthrough") {
    None
} else { detect_family(&route.path) };
```

`family = None` goes straight to `Prepared::passthrough`. An omitted field now
selects path detection, while an explicit `"passthrough"` still relays the body
unchanged. The historical failure measured 98,676 bytes in and out with zero
savings; the preflight remains useful for detecting an unrecognised path or a
fully frozen prompt.

## Watching a run

```bash
make harbor-watch                      # newest run, refreshes every 5s
scripts/harbor/watch.py <run-dir> --once
```

It reads the statistics database read-only through a `file:…?mode=ro` URI, so
it is safe against a run in progress, and it notices when the port belongs to a
*later* run rather than attributing that proxy's live numbers to this one.

By hand — note the address is wherever the proxy was bound for the containers
to reach it, **not** localhost:

```bash
A=$(sed -n 's/^listen = "\(.*\)"/\1/p' <run-dir>/kompressor.toml)
curl -s $A/v1/stats/summary    | jq    # gross, net, retrievals, hit rate
curl -s $A/v1/stats/by_type    | jq    # which compressors are earning
curl -s $A/v1/stats/store      | jq    # fragments and bytes retained right now
curl -s $A/v1/config                   # TOML, not JSON
curl -s $A/metrics                     # refs live/bytes, and events DROPPED
```

`kompressor_stats_dropped_total` is the first thing to check before quoting any
number: non-zero means the queue overflowed and every report is an undercount.
`/v1/stats/requests` is a single-request lookup needing `?request_id=…`; it does
not list.

The CLI reads the same database directly:

```bash
S=<run-dir>/kompressor-stats.db
kompressor stats summary --stats $S
kompressor stats by-type --stats $S
kompressor stats request <request_id> --stats $S
```

## Reading the numbers

**Join on `request_id`.** `proxy_requests.compression_event_id` is declared as
a foreign key and deliberately never written; joining on it reports a fully
compressing run as having compressed nothing.

```sql
SELECT pr.time_ms, oe.tokens_in, oe.tokens_out, oe.refs_emitted,
       pr.prompt_tokens, pr.cache_read_tokens, pr.retrieve_rounds, pr.error
  FROM proxy_requests pr
  LEFT JOIN optimization_events oe
         ON oe.request_id = pr.request_id AND oe.surface = 'proxy'
 ORDER BY pr.time_ms;
```

**Prefer the marginal figure.** A windowed sum of gross over-counts on agent
traffic: every turn re-compresses the same growing history and the same bytes
are credited again. `harbor/watch.py` reports both — the windowed total and how
much of each turn's *new* context was removed. When they disagree sharply,
the marginal one is the one that survives being quoted.

**A 200 with no usage is not the same as a 200 reporting zero.** The validity
section distinguishes them, and for a while it did not. `prompt_tokens` is an
`Option<u32>` filled from the response's own usage: **NULL** means no usage
reached the proxy at all, which is what a mid-stream quota or policy refusal
looks like — the provider sends headers before it knows it will refuse, so the
status code is 200 and nothing downstream can see the failure. That is the
condition worth stopping a run over; it was measured at 17 of 20 trials dead
while the summary reported 143/143 OK.

An explicit **0** is a different and much weaker thing: a usage object parsed
and reported nothing. One of those turned up on 2026-10-06 — a single
non-streaming preflight probe, 1.3s like every healthy run's probe, with the
other 31 requests normal and the task resolved — and the old predicate, which
read NULL and 0 alike, called the whole run void. Zero-usage requests are now
counted separately and only raise a flag above `ZERO_USAGE_SHARE` (10% of the
run), because one probe is noise and most of a run is the same quota failure in
another shape. The no-usage flag also no longer offers the mid-stream
explanation when none of the affected requests streamed.

**Capturing the bodies of a live run.** Every benchmark run writes
`kompressor-bodies.jsonl` into its own dump: one JSON object per line, every
request both ways — as it arrived from the agent and as it left for the
provider — bodies whole and uncompressed.

Kept with the run rather than in `/tmp`, and kept on passing runs too, because a
failing run's bodies are only interpretable against a passing run's. That lesson
cost something: the first accuracy failure in twenty runs arrived with its own
capture and nothing to compare it against, the previous run's having been
deleted.

Set `KOMPRESSOR_BODY_CAPTURE` to put it somewhere else; the proxy honours the
variable directly, so it also works outside the bench.

The same file also carries the **relay's decision about each streamed response
frame**: state, signal, whether the turn could retrieve, the running content
total and the grace it is measured against — and, when the relay gives up, a
line saying so with the totals that decided it and, in words, **which rule
fired**. That one is stated rather than implied, because once the relay is in
passthrough it stops observing and the trace would otherwise just stop, leaving
an absence to explain the one event it exists for.

The reason is written out because the totals alone mislead. The first capture
of a live run held 28 give-ups whose content totals were 0-299 bytes against a
grace of 8,192 — which reads as a relay abandoning turns far inside its own
bound, and is not what happened: all 28 were `OtherCall`, the model calling a
tool the client declared, where the turn is the client's and giving up is the
designed behaviour. The record printed the two content numbers on every
give-up, which implied the content bound was involved even when no content rule
had been consulted. A trace that has to be cross-checked against the preceding
signal to be read correctly is a trace that will be read incorrectly.

This answers what the request bodies cannot: a `optscale_kompressor_retrieve` call that
reaches the client did so because the turn had already gone passthrough, and
only the running content total says why. `log.payloads` traces the same fields
at debug level and has for some time, but it is all-or-nothing — it also logs
retrieved fragments, which are tool output and can hold anything the command
printed — so it is not something to leave on. This records the decision without
the contents.

The relayed websocket path has its own, `KOMPRESSOR_WS_CAPTURE`, written to
`kompressor-ws-frames.jsonl` **in the run's own dump** beside the bodies — the
benchmark names it on every run, and the file only appears when
`proxy.websocket_relay` is on. It used to default nowhere, so a relayed run's
frames went to `/tmp` if anywhere, were overwritten by the next run, and were
not beside the bodies they have to be read against. A capture kept somewhere
else is a capture nobody has when the question comes up. Setting the variable
still wins, for a run that wants it somewhere specific.

The codex sweep names it too, under the same filename in its own `$WORK`. It
was the later of the two and the one that mattered more: `WEBSOCKET=1` on
that script is the only path in the tree that actually puts codex on a
relayed socket, so until 2026-10-09 the run that exercised the relay was the
run that captured nothing. That script now pins the relay in **both**
directions rather than only switching it on, because the proxy's own default
moved: leaving the variable out no longer means "no relay", it means whatever
the installed config resolves to, and a sweep has to measure the transport it
says it measures.
`report` counts the frames it finds and how many carry handles, which is the
cheapest available answer to "did this run go over the relay at all" — a
relayed sweep reports no provider usage, so the token table cannot answer it.

**A Harbor run captures no frames, and that is not a fault.** The relay now
ships on, so every Harbor proxy is relay-capable without anyone asking — and it
still writes nothing, because the setting says the proxy *will* relay a
websocket and cannot make a client open one. `suite.sh` hands its codex agent
`OPENAI_BASE_URL` and an API key, which is the plain HTTP Responses path, while
codex only reaches for a websocket on its ChatGPT-subscription login — the path
`wrap` takes and Harbor never does. Measured across five relay-enabled Harbor
runs, back when enabling it was a decision: **188 requests, zero 101
handshakes, no frame file written**, while every `WEBSOCKET=1` sweep produces
one 101 per session. That is also why flipping the default changed no Harbor
number — a run that needs frames is a sweep, not a Harbor stage. If a future
codex build does open a socket on an API key, the frames land in the run folder
on their own.

Naming the variable was not enough on its own. `setup` adopts a proxy already
listening on the port when it is running the current binary, and an adopted
proxy keeps the environment it was started with — so the first relayed sweep
after the capture was added still wrote no frames, against a proxy that had
been up for eighty minutes. The inode check answers which binary; it had
nothing to say about which settings. `start_proxy` now records them beside
`serve.log` and `setup` restarts when they differ, which also ends a quieter
version of the same fault: `bughunt` raises `retrieve_max_rounds` to 16 and
used to leave the proxy that way for every stage that followed it.

What the marker records is the **resolved configuration**, asked of
`check-config`, not the inputs that produce it: one digest over that output,
plus the relay as a readable line. Hashing the file and the ambient environment
separately was the first attempt and both were the wrong thing to hash — the
environment reaches the proxy without appearing in the marker, and a file
digest counts comments, so editing a comment restarted a working proxy for a
change that could not affect it. Secrets are redacted in that output, so a
changed `auth.secret` does not move the digest; this script does not use auth.

The relay line is the **effective** relay, not the `WEBSOCKET=1` override. The first version recorded the override and so
reported two identical proxies as different: a run without `WEBSOCKET=1`,
against a config file that carries `websocket_relay = true` on its own,
restarted a working proxy and printed a diff implying the relay was about to go
off. It stayed on and the run relayed — the restart changed nothing and the
message said otherwise, which is worse than the wasted restart. A marker exists
to answer "will this proxy behave as this run needs", and only the resolved
setting answers that.

It records frames rather than bodies, and in three directions: `client->proxy` as the frame
arrived, `proxy->upstream` as it was sent, and `upstream->client` coming back.
Until 2026-10-07 it recorded only the arriving frame, under the label
`client->upstream` — which named the frame's destination rather than its
content, and left a capture with no record of the bytes actually sent. That is
the wrong side for asking which byte of a prefix moved, which is the only
question these captures exist for. Both write through one serialized appender:
the relay's two pump directions run concurrently by construction, and
unserialized appends interleave mid-line until the file stops being JSON.

Two things to know. The redaction sweep covers the dump, so the capture is swept
with everything else — but it skips files over 64 MB and now says so loudly when
it does, because a prompt dump too large to redact is one to check before
copying anywhere. And a run is roughly 13 MB, so a long series accumulates.

It exists for the one question the aggregates cannot answer: *which byte
changed*. Join it to `proxy_requests` on `request_id` for the conversation, the
cache reads and the turn ordering, find the turn where `cache_read_tokens` fell,
and diff that turn's outgoing body against the previous turn's. Both directions
are recorded because the discriminator is **who** changed the byte: if the
*incoming* bodies already differ at the position the cache fell back to, the
agent rewrote its own history and the proxy is not at fault.

Expect a few megabytes per run — roughly seventy requests of 15–80 KB each way.
The bodies are not truncated, since truncating them removes the only thing the
file is for. Writes are serialized against each other: a proxy serves requests
concurrently, and unserialized appends interleave mid-line and stop the file
being JSON.

**Run `scripts/harbor/cache-loss-diff.py <run-dir>` when the flag fires.** It
does what the flag describes — joins the captured bodies to `proxy_requests`,
compares tools, instructions and the conversation separately, names the first
differing message, and exits non-zero if the loss was ours. It exists because
the procedure was prose for long enough to be retyped by hand each time, and a
hand-retyped procedure is subtly different each time.

It reads `proxy_requests` joined to `optimization_events`, not the proxy table
alone. `cache_losses` is shared with the benchmark and one of its exclusions
keys on `messages_total`, which lives on the engine's row — so reading the
proxy table alone left this tool silently running two exclusions where the
benchmark ran three. Measured on the 89-task run of 2026-10-13: a comparison
that went `messages 92 -> 5`, a second agent session filed under the same
conversation fingerprint, was reported as 27,008 tokens lost and attributed to
the proxy. A shared function is only shared if both callers hand it the same
shape.

Non-zero means *ours*, not *wrong*. On run 2026-10-12 09:54 it separated three
losses in one run — two the provider's, one a promotion of a twice-fetched
fragment that cost 1,024 tokens and saves 1,047 per fetch it prevents, so it
paid for itself on the next one (§12.6). A promotion is a priced decision and
the tool says so in its closing line; a **demotion**, markers 0 → 1, is the one
that is always a bug. Read the direction before reading the exit code.

Check the exit code without a pipe. `python3 cache-loss-diff.py <dir> | tail`
reports `tail`'s status, not the tool's, which is how three runs were recorded
as clean when one of them was not.

It decides promotion from demotion by **counting `ref:k_` markers**, not by
looking for marker wording. The obvious phrase to match on,
`[... N body lines omitted from NAME; ...]`, is only the *code* compressor's;
every other one leaves a bare `ref:k_…`. Matching the phrase therefore missed a
textbook promotion on 2026-10-09, where a config payload went from one marker to
10,663 characters of inline text and the direction read as "neither". The
distinction is worth getting right: a promotion is a decision the engine is
allowed to take and has priced, while **a demotion is always a bug** — it means
the decision was re-taken against a bar that had moved and the prefix was
rewritten twice.

**A cache loss invalidates the cost, and the run will not say so by itself.**
A prefix is append-only, so the part the provider serves from cache can only
grow. When it shrinks, everything after the first missing byte is billed at full
price again — the request still succeeds, the agent is unaffected, and the only
trace is a cost that no longer compares with the run beside it. The validity
section flags it (`PROVIDER CACHE LOSS`), counting a drop only *within* one
conversation — two conversations interleaved make a sawtooth that is not a loss
— and treating a retrieve-round row as something to **step over** rather than
compare. Such a row aggregates every submission of its turn, so the ordinary
turn after it is always lower and comparing the two reports a loss that did not
happen: run A of the 2026-10-05 smokes would show 62,976 tokens gone. But the
prefix still only grows across that turn, so the last ordinary row before it
remains a valid baseline for the next ordinary row after it.

Discarding the turn outright, which this did until 2026-10-07, left a blind
spot: one run of sixteen had its first-compression turn run two retrieve rounds
and was reported clean on that basis alone. Closing it changed none of the
sixteen totals — that run really was clean, 11,648 cached before the round and
16,768 after — so the fix bought soundness rather than numbers. That is the
outcome a measurement fix should usually have.

It is reported on every run, zero included, as `cache lost`, directly under
`billed prompt`. Always, because a line that appears only when something is
wrong cannot tell a reader that a run was clean — its absence reads the same as
a build of this script that never measured it.

```
cache lost       0 tokens over 0 turn(s) (the prefix only ever grew)
cache lost       43,008 tokens over 4 turn(s) -- 27.1% of everything billed at full price here
```

`withheld` is the opposite convention and deliberately so: printed only when
non-zero, because unlike cache loss it is not a property of the run that a
reader needs to confirm was clean, and a `0 withheld` line on every report
would invite the question of what it is for. When it appears, the model asked
for a fragment and `proxy.retrieve_max_tokens_per_request` was already spent,
so the store was never consulted:

```
retrievals       8 (8 hits, 1 narrowed) over 5 rounds
withheld         1 refused before the store was read -- ...
```

Those rows are excluded from `retrievals`, `hits` and `narrowed` above them. A
refusal and a miss are both `found = 0` in `retrieval_events` and separated
only by schema v10's `withheld` column; counting them together reported `12 (11
hits)` for the Harbor run of 2026-10-09 15:56, in which all eleven asks that
reached the store hit and the twelfth was never put to it. Two consecutive
smoke runs have since had the budget bind exactly once each — on the HTTP path,
where it had never been visible.

Measured over six smoke runs of one task: three clean, three not — two single
losses of 9,216 tokens and one run with four totalling 43,008. Every cache-read
value the provider reports is a multiple of **128**, its cache block size; all
49 distinct values across these runs are, with no exceptions. An earlier note
here claimed the *losses* were always multiples of 1024 — six of six have been,
but that is a small sample and partly not independent, so treat it as an
observation rather than a rule.

**What causes it is not settled, and an earlier version of this section said it
was.** Two readings fit:

* *The provider.* Every drop arrives seconds rather than minutes after the
  previous turn — 4.7s to 14.1s — which is far too fast for a cache TTL to
  expire.
* *This proxy.* Runs C and F produced the **identical** transition, `11,648 ->
  2,432` losing 9,216, at the same early point of the same task. A fall back to
  a fixed offset is precisely what a rewritten prefix looks like, and the engine
  reports `0 inside the frozen prefix` on every one of these runs — it is free
  to rewrite anything, every turn.
* *Against that reading*, three runs compressed from the same turn onward and
  never dropped at all, and one run's four losses arrived late and scattered
  (turns 19, 22, 30, 38) rather than at a structural boundary.

**Resolved: two causes in this proxy, both fixed, and a third that is the
provider's.** Every one was settled by diffing the captured bodies at the losing
turn. None was settled by reasoning from the summary numbers — several confident
readings taken from aggregates alone had to be retracted first, including one in
an earlier version of this section.

| cause | evidence | status |
|---|---|---|
| the tools block appearing mid-conversation | at one turn the conversation was byte-identical and the agent's own tools unchanged, while ours went 10,622 → 13,207 bytes as `kompressor_retrieve` was added | fixed: offered from the first request of a recoverable conversation |
| a keep-inline promotion rewriting history | a 175-token fragment promoted 17% into the prompt; `cache_read` 18,816 → 12,672, handles 11 → 7 on that turn | priced: refused when the break cannot pay back |
| the provider | tools, instructions and conversation all diff clean; seen as 16,768 → 0 recovering to 19,840 the next turn, and as 1,024 | not ours, and not preventable |

**The magnitude does not identify the cause.** `11,648 → 10,624` was the tools
block five times, and after the fix it happened once more with every byte we
sent identical in both directions. Diff, do not infer — which is why the body
capture is on by default.

Two earlier claims here were wrong and are worth recording as such. "Nine of ten
runs land on exactly 2,432, that is the position of the first byte this proxy
rewrites" — 2,432 is one of three landings (10,624 and 0 are the others) and not
a fixed offset. And a test that pooled every loss together found 57% of loss
turns against 54% of non-loss turns had compressed more, which reads as no
signal only because it mixed two populations that cancel.

The cost is not marginal. In the run with four losses, those 43,008 tokens were
**27% of every full-price token billed**, and that run came out the most
expensive per token of the six ($1.205/MTok against $0.945 for the cleanest)
while removing a perfectly ordinary 47.4%. Read without this line it looks like
a worse compression run; it is a worse *cache* run. The proxy logs the same
event at `warn` from `prefixcache`.

`make bench-selftest` runs the reader's own checks for this — no build, no
provider, no spend — and `make check` includes it.

**Watch the cache share.** A measured smoke removed 54% of the compressible
text while 91% of the billed prompt was a provider cache read. Tokens removed
from a prefix that was already being served at cache rates are worth much less
than the ratio suggests.

**Count what was retained against what was fetched.** The same run minted 1,152
handles and 6 of them were ever asked for — 0.52%, none narrowed. That is the
number that says whether `recoverable` is earning its retention on this
workload.

**A resolved rate needs trials, and one is not a trial.** The smoke runs a
single task, so `resolved 1/1 (100.0%)` is the most misread line the harness
prints: it is one Bernoulli draw from a task that passes most of the time by
itself. Counted across every `mailman` trial on disk — 32 of them, spanning
`recoverable`, fold on and off, headroom, and two runs with compression
disabled outright — the task resolves **25/32, 78.1%**, and the
compression-off runs are among the passes. So a single pass is the expected
outcome whether or not this proxy is in the path, and a single failure is
within the ordinary spread.

What a resolved rate can support is a *paired* comparison: the same task list,
the same model, mode on and off, enough trials that the difference exceeds the
task's own variance. Until then the honest reading of a green smoke is "nothing
broke", which is worth having and is not an accuracy result. The compression
figures in the same report do not share this problem — they are measured per
request against what the provider received, so one run of 78 requests is 78
observations rather than one.

| watch for | what it means |
|---|---|
| `abandoned` climbing | the client is hanging up mid-stream; those tokens were billed and reached nobody |
| `secs` high with `rnds > 0` | the retrieve loop is stacking upstream round trips inside one client request |
| `cached` near zero after the first turns | the prompt cache is being busted, which costs more than compression saves |
| many handles minted, few fetched | retention is being paid for and not used |
| statistics events dropped | every number is an undercount |

## Offline, before spending anything

`scripts/harbor/lab/` pushes one codex-shaped Responses turn through the proxy
at an upstream that records exactly what it received, so the forwarded bytes can
be diffed against the sent bytes:

```bash
make harbor-lab
```

It checks the things that would be expensive to discover on a paid run: that
`instructions` and the `reasoning`/`encrypted_content` items come through byte
for byte, that `store`/`include`/`prompt_cache_key` survive, that the retrieve
tool is appended after the client's own tools, and that the retrieve loop runs
to completion over SSE without the client ever seeing the call.

`slow_upstream.py` and `first_byte.py` measure the other thing a corpus cannot:
**when the client first sees a byte.** Against an upstream that sends
`response.created` immediately and then thinks for 25 seconds, a direct client
sees its first byte at +0.0s. Through the proxy it sees one at +10.0s — the
keep-alive interval — because the relay holds frames back while it decides
whose turn the stream is. Before that keep-alive existed the client saw nothing
for the full 25s, and a real agent gave up after five minutes of it.
