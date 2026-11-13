# Wrapping Codex CLI

```bash
kompressor wrap codex -- exec "summarise the last build failure"
```

That works, and this document explains what it does to make it work, because
the mechanism edits a file outside this repository and you should know which
one before you run it.

Everything below was measured against **codex-cli 0.155.1** on Linux. Codex
changes its transport and its configuration schema between releases, so check
the [Verify it is working](#verify-it-is-working) section after any codex
upgrade rather than trusting that a working setup stayed working.

## Contents

- [Why a variable is not enough](#why-a-variable-is-not-enough)
- [What the wrap writes](#what-the-wrap-writes)
- [Using a ChatGPT subscription](#using-a-chatgpt-subscription)
- [Permanent setup](#permanent-setup)
- [Measuring it](#measuring-it)
- [Verify it is working](#verify-it-is-working)
- [Tuning](#tuning)
- [What to expect](#what-to-expect)
- [Real sessions, by payload type](#real-sessions-by-payload-type)
- [Known limits](#known-limits)
- [Troubleshooting](#troubleshooting)

## Why a variable is not enough

`kompressor wrap` normally works by setting a base-URL environment variable.
Codex is the entry in the catalogue that cannot be wrapped that way, for three
separate reasons — each one on its own enough to reduce compression to zero.

**1. Codex ignores `OPENAI_BASE_URL`.** Measured: exporting it and running
`codex exec` sent every request straight to the provider, and a listener on the
configured port recorded nothing at all. So `wrap codex` sets no variable; it
writes the config file instead.

**2. Codex prefers a WebSocket transport.** The built-in `openai` provider
opens `ws://…/v1/responses` before trying HTTPS. Since 0.14.0 OptScale Kompressor
relays that upgrade and compresses the `response.create` frames on it
(`proxy.websocket_relay`, on by default, §12.5b); what it cannot do on that
transport is read a `response.completed`, so a relayed turn reports no provider
usage and its savings are real but uncounted.

Before the relay existed there was no server-side cure. The handshake was
retried five times before codex gave up — measured at roughly 13 seconds of
dead air per session against a listener that refused the upgrade, and refusing
it with `404` and with `501` made no difference. For an **API-key** session the
wrap still declares `supports_websockets = false` on the provider block it
writes, which suppresses the attempt entirely: measured 6 POSTs, no handshake,
no delay. A **subscription** session goes through the built-in provider, where
no top-level key can disable the preference, and that is the case the relay was
built for.

**3. The subscription endpoint used to be invisible to the family detector.**
With a ChatGPT login, codex posts to `/backend-api/codex/responses`.
`detect_family` matched `…/v1/responses` or exactly `/responses`, and that path
is neither — so the request became a passthrough and the body was never parsed.
Measured on a 77 KB payload, same config in every other respect:

| forwarded path | recorded family | `x-kompressor-saved-tokens` |
|---|---|---|
| `/backend-api/codex/responses` | `passthrough` | **0** |
| `/responses` | `openai_responses` | **25992** |

`detect_family` now recognises `…/codex/responses` and `/backend-api/responses`
directly, so this no longer needs working around. The prefix-stripping trick
below still works and is still what the older recipe used.

## What the wrap writes

Under a ChatGPT login, nothing: `wrap` passes `-c openai_base_url=…` on codex's
command line instead, for the reason under
[Using a ChatGPT subscription](#using-a-chatgpt-subscription). It does remove
a block a killed session left in the file, unless a live session holds it.

Under an API key, `kompressor wrap codex` adds two marker-delimited blocks to
`$CODEX_HOME/config.toml` (default `~/.codex/config.toml`) and removes them
when the session exits:

```toml
# --- kompressor wrap: managed block, removed on exit ---
model_provider = "kompressor"
openai_base_url = "http://127.0.0.1:8817/v1"
# --- end kompressor wrap ---

  ...your configuration, untouched...

# --- kompressor wrap: managed block, removed on exit ---
[model_providers.kompressor]
name = "OpenAI via kompressor"
base_url = "http://127.0.0.1:8817/v1"
wire_api = "responses"
env_key = "OPENAI_API_KEY"
supports_websockets = false
# --- end kompressor wrap ---
```

Five things about that are deliberate:

- **Two blocks, head and foot.** A bare TOML key belongs to the table that
  precedes it, so a top-level key appended after your `[model_providers.…]`
  section would silently land *inside* it.
- **The edit is textual, not a TOML round-trip.** Everything outside the
  markers is preserved byte for byte, including your comments and your
  ordering, which a parse-and-re-render could not promise.
- **Both `model_provider` and `openai_base_url`.** `model_provider` selects
  our mount; `openai_base_url` points the *built-in* provider here too, so
  nothing in the session reaches OpenAI past the proxy.
- **A key you already set is a refusal, not an overwrite.** TOML rejects
  duplicate keys, so appending ours would not win the argument; it would stop
  codex reading the file at all. The wrap declines and names the key.
- **`env_key` or `requires_openai_auth`.** For a key stored by
  `codex login --with-api-key` the wrap writes `requires_openai_auth = true`
  instead of `env_key`: each finds only its own kind of key (codex 0.164.0).

If a session is killed before it can clean up, `kompressor unwrap codex`
removes the blocks. Concurrent wraps are reference-counted: the last one out
removes the managed blocks, and a claim counts only while its owner is alive.
Settings and comments edited outside those blocks during a session survive
cleanup. If nothing changed, cleanup restores the original file byte for byte;
a file created by the wrap is removed only when no user edits were added.

## Using a ChatGPT subscription

![codex on a ChatGPT subscription, through the proxy](charts/chatgpt-subscription.drawio.png)

Your codex install keeps using your own login and talks to OpenAI as it always
did; OptScale Kompressor sits in front of it on loopback and rewrites the prompt bodies.
Deciding whether you want your subscription traffic rewritten before it is sent
is your call, not this document's.

`kompressor wrap codex` handles this case too, and two things make it work.

**The `openai_base_url` key, as a flag.** Codex routes subscription traffic
through its built-in `openai` provider regardless of `model_provider`, so the
custom provider block alone would not catch it. The top-level override does.
`wrap` passes it on codex's command line, `-c openai_base_url="http://…/v1"`,
and does not touch `config.toml` for a ChatGPT login. That is not a matter of
taste: measured on codex 0.164.0, the same key written into `config.toml` makes
codex send an API key (`sk-…`) of its own in place of the plan's token, and
api.openai.com refuses that key with `401 … Missing scopes:
api.responses.write`. As a flag, codex keeps the plan's token and adds a
`chatgpt-account-id` header. Leaving the file alone also keeps the Codex
desktop app, which reads the same file, off the proxy.

**An upstream that follows the credential.** A ChatGPT plan puts the same
Responses body on a different host under a different prefix:
`chatgpt.com/backend-api/codex/responses` rather than
`api.openai.com/v1/responses`. Only the credential distinguishes them, so the
`openai` mount carries a `[upstreams.subscription]` override, and the proxy
selects it when the caller's headers classify as a subscription:

```toml
[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "https://api.openai.com"
api_family = "openai"
forward_auth = true

# Where the same mount's traffic goes when the caller pays by subscription.
[proxy.upstreams.subscription]
base_url = "https://chatgpt.com"
rewrite_prefix = "/backend-api/codex"
```

This matters more than it looks. Subscription-mode codex polls rate limits,
agent identity and JWT refresh under `/backend-api/*`; forwarding those to
`api.openai.com` returns 404, and codex reads a 404 on that surface as an
invalid session and **stops using the subscription at all**. If you configure
OptScale Kompressor by hand rather than through `wrap`, add a passthrough mount for the
rest of that surface:

```toml
[[proxy.upstreams]]
name = "chatgpt-rest"
path_prefix = "/backend-api"
base_url = "https://chatgpt.com"
api_family = "passthrough"
forward_auth = true
```

> **What a subscription request looks like.** Measured on codex 0.164.0 under a
> ChatGPT login, given the flag: a WebSocket on `/v1/responses`, or
> `POST /v1/responses`, carrying the plan's token (a JWT) and a
> `chatgpt-account-id` header. The agent string is `codex-tui/…` in a terminal
> (`codex_cli_rs/…` up to 0.162) and `codex_exec/…` for `codex exec`. The proxy
> treats a request as a subscription when it carries that header, or a known
> Codex agent string, and no API key.
>
> `401 … Missing scopes: api.responses.write` means a request reached
> api.openai.com with an API key that may not use the Responses API. Either
> codex was given `openai_base_url` in its config file under a ChatGPT login
> (above) — the next `wrap codex` removes a block left by an older `wrap`, as
> does `kompressor unwrap codex` —
> or it is logged in with a restricted key: give that key write access to the
> Responses API in the OpenAI dashboard, or use an unrestricted one.

There is one cost you cannot avoid on this path. `openai_base_url` overrides
the built-in provider's URL but cannot disable its WebSocket preference, and
there is no top-level key that can — so a subscription session pays the
five-attempt handshake before falling back to HTTPS. An API-key session, which
goes through the provider block, does not.

## Permanent setup

`kompressor wrap codex` is per-session: it points codex at the proxy, runs it,
and leaves nothing behind. To leave compression on for every codex session, run
`kompressor serve` as a service and write the codex side once.

Needs **OptScale Kompressor 0.3.0 or newer**. Earlier versions cannot read codex's
request bodies (it sends `content-encoding: zstd`) and do not recognise the
subscription response stream, so they compress nothing.

### 1. OptScale Kompressor

`~/.config/kompressor/config.toml`:

```toml
[server]
listen = "127.0.0.1:8817"

[engine]
mode = "recoverable"
# This is the default since 0.4.1; earlier versions shipped 64 KiB, which
# silently dropped recoverability on exactly the payloads worth compressing.
max_ref_bytes = 1048576

[refs]
backend = "sqlite"
path = "/home/you/.local/share/kompressor/refs.db"

[stats]
enabled = true
path = "/home/you/.local/share/kompressor/stats.db"

[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "https://api.openai.com"
api_family = "openai"
forward_auth = true

# Where the same mount goes when the caller pays by subscription. Omit this and
# a ChatGPT-plan session is forwarded to api.openai.com, which 404s — and codex
# reads a 404 on that surface as an invalid session.
[proxy.upstreams.subscription]
base_url = "https://chatgpt.com"
rewrite_prefix = "/backend-api/codex"
```

`api_family` defaults to `"auto"`, which detects known endpoints from their
forwarded path. `"passthrough"` is an explicit opt-out of parsing.

Only the `/v1` mount is needed. Codex sends `/v1/responses` and `/v1/models` to
whatever base URL it is given and talks to `chatgpt.com` directly for
everything else, so the proxy never sees the rest of the `/backend-api`
surface — verified by recording every request of a live session.

```bash
kompressor serve --proxy --api          # or the systemd unit below
```

### 2. codex — pick the block for your login

Check which you have:

```bash
python3 -c "import json,pathlib;print(json.loads((pathlib.Path.home()/'.codex/auth.json').read_text()).get('auth_mode'))"
```

**`chatgpt` (a ChatGPT plan).** One key, given on codex's command line, not in
`~/.codex/config.toml`: from codex 0.164.0 the key in the file makes codex send
an API key instead of the plan's token, and every request fails with
`401 … Missing scopes: api.responses.write` (see
[Using a ChatGPT subscription](#using-a-chatgpt-subscription)). A shell alias
does it for every session:

```bash
alias codex='codex -c openai_base_url="http://127.0.0.1:8817/v1"'
```

or, in a PowerShell profile (`notepad $PROFILE`):

```powershell
function codex { codex.exe -c 'openai_base_url="http://127.0.0.1:8817/v1"' @args }
```

That is the whole change, and the omissions are deliberate. Do **not** add
`model_provider` or a `[model_providers.*]` block here: codex honours
`model_provider` under a ChatGPT plan, so it selects that provider, finds the
`env_key` it declares, and refuses the session with
`Missing environment variable: OPENAI_API_KEY` before sending anything.

**`apikey`.** A custom provider, which is the better shape when it works —
it is the only place `supports_websockets` can be set:

```toml
model_provider = "kompressor"
openai_base_url = "http://127.0.0.1:8817/v1"

[model_providers.kompressor]
name = "OpenAI via kompressor"
base_url = "http://127.0.0.1:8817/v1"
wire_api = "responses"
env_key = "OPENAI_API_KEY"
supports_websockets = false
```

The id cannot be `openai` — codex refuses to redefine a built-in provider.
For a key stored by `codex login --with-api-key`, use
`requires_openai_auth = true` instead of `env_key`, which reads only the
environment.

### 3. Keep it running

```ini
# ~/.config/systemd/user/kompressor.service
[Unit]
Description=kompressor compression proxy
After=network-online.target

[Service]
ExecStart=%h/.local/bin/kompressor serve --proxy --api
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now kompressor.service
```

### 4. Undoing it

Delete the keys you added from `~/.codex/config.toml`, or the alias. A block left behind by
an interrupted `kompressor wrap` is removed by `kompressor unwrap codex`, which
is safe to run whether or not one is there.

### What it costs on the subscription path

A ChatGPT-plan session opens a WebSocket it cannot be told not to:
`openai_base_url` overrides the built-in provider's URL but cannot disable its
transport preference, and there is no top-level key that can. Codex opens
`ws://127.0.0.1:8817/v1/responses` before trying HTTPS.

**The relay answers it**, and has since 0.14.0 (`proxy.websocket_relay`, on by
default). The cost is accounting rather than latency: the relay never reads a
`response.completed`, so a relayed session's token counts are filed null and no
cost report can price it. The saving is real and uncounted.

Turning the relay off restores the old behaviour, which is the shape this
section used to describe: five refused handshakes, about 13 seconds of dead air
on the first request, then a fallback to HTTP. That is a supported choice for an
operator who would rather have the numbers —
`KOMPRESSOR__PROXY__WEBSOCKET_RELAY=false` — not a default anyone should meet by
surprise.

An API-key session goes through the provider block, where
`supports_websockets = false` removes the attempt entirely.

## Measuring it

Compression that cannot be measured is a claim. With `[stats] enabled = true`
every request, compression and retrieval is recorded, and `kompressor stats`
reads them back. All of these take `--from` (`7d`, `2026-09-01`, epoch ms),
`--to`, and `--format json`.

**Is it working, and what is it saving?**

```bash
kompressor stats summary --from 1d
```

```
requests            58
messages            225 total, 34 compressed
tokens              572690 in, 287136 out
saved gross         285554
retrieved back      3383 over 3 retrievals (3 hits, 100.0% hit rate)
saved net           282171
  mode recoverable  56 requests, 285554 gross
  from proxy        58 requests, 285554 gross
```

**saved net** is the number to quote: gross saving minus what the retrieve loop
put back. `requests 0` means codex never reached the proxy. A high request
count with `0 compressed` means it arrived and nothing was eligible.

**Which payloads are paying for themselves?**

```bash
kompressor stats by-type --from 7d
```

```
type              messages     share
search                  22    56.41%
listing                  9    23.08%
text                     4    10.26%
source_code              2     5.13%
```

This is where compression earns its keep or does not: `listing`, `search` and
`log` compress heavily, `source_code` and `text` barely at all. A session
dominated by the latter will show a small saving, and that is the corpus rather
than a fault.

**Which upstream is this, and where did it actually go?**

```bash
kompressor stats by-provider --from 7d
```

```
provider        requests     tokens in       removed    share  sent to
openai                71       1834902       1211455   66.02%  https://chatgpt.com
```

The provider is the mount's own name from your configuration, not a host,
because one mount sends to more than one: the same `openai` mount goes to
`api.openai.com` on an API key and to `chatgpt.com` under a subscription. The
last column is where the bytes went, which is how you confirm the subscription
override is actually firing — a ChatGPT-plan session showing `api.openai.com`
there is being routed as pay-as-you-go. `(unnamed)` is traffic from a surface
with no mount, or recorded before 0.4.1.

**Is recoverability being used, and does it resolve?**

```bash
kompressor stats retrievals --from 7d
```

A hit rate below 100% means the model asked for a fragment that had expired or
was never retained — raise `engine.max_ref_bytes` or `engine.ref_ttl_seconds`.

A **`withheld`** line is a different failure and is counted outside that rate:
the model asked, and `proxy.retrieve_max_tokens_per_request` was already spent,
so the store was never consulted. The fragment may well have been there. A 100%
hit rate beside a non-zero `withheld` is a true statement about the store and a
misleading one about the request, which is why the two are never added
together. `kompressor stats summary` shows the rate but not the refusals, so a
run with any `withheld` needs this sub-report to be read properly.

**One request end to end**, joining its compression and its retrievals:

```bash
kompressor stats request <request-id>       # the x-kompressor-request-id header
```

**Per-session, without any of the above.** `kompressor wrap` prints a verdict
when the agent exits, scoped to that session alone:

```
kompressor: 13 request(s), 5 message(s) compressed, ~57690 tokens saved (54656 net of retrieval)
```

**One page with all of it**, which is the form to use when compression has been
running for days behind an agent and the question is whether to keep it:

```bash
kompressor stats report --from 7d > report.html
```

A self-contained HTML file. The headline cards read in the order the
arithmetic does — tokens in, net, retrieved, gross — because
`net = gross − retrieved`, charging only the retrievals that found their
fragment. Then a savings-per-day chart, the per-type split, the retrieval side,
and a footer explaining what the numbers do and do not mean. No JavaScript, no web fonts, no CDN — it opens
from `file://` on a machine with no network and still renders in a year, which
matters because the point of writing it to disk is to keep it and compare.
`--bucket hour` for a tighter trend.

To have one waiting each morning:

```ini
# ~/.config/systemd/user/kompressor-report.service
[Service]
Type=oneshot
ExecStart=/bin/sh -c '%h/.local/bin/kompressor stats report --from 7d > %h/kompressor-report.html'

# ~/.config/systemd/user/kompressor-report.timer
[Timer]
OnCalendar=daily
Persistent=true
[Install]
WantedBy=timers.target
```

```bash
systemctl --user enable --now kompressor-report.timer
```

**Anything the reports do not cover** is in the database, which is ordinary
SQLite with eight views over it:

```bash
sqlite3 ~/.local/share/kompressor/stats.db \
  "select content_type, count(*), round(avg(100.0*(chars_in-chars_out)/chars_in),1) as pct
     from optimization_events group by 1 order by 2 desc;"
```

`v_request_economy`, `v_type_coverage`, `v_daily_savings`, `v_outcomes`,
`v_retrieval_hit_rate`, `v_skip_reasons`, `v_conversations` and
`v_daily_requests` are the shipped ones. `v_retrieval_hit_rate` carries a
`withheld` column beside its rate, and excludes those rows from the rate
itself.

## Verify it is working

The failure mode is silence, so check rather than assume.

`kompressor wrap codex` counts what reached the proxy and says so on exit, and
that verdict is the quickest check there is:

```
kompressor: 24 request(s), 11 message(s) compressed, ~106271 tokens saved (105729 net of retrieval)
```

`no requests reached the proxy` there means the wiring did not take. A
hand-configured `kompressor serve` prints no such verdict, so check the
statistics instead. Run one task, then:

```bash
kompressor stats summary
```

A working setup reports a non-zero request count and a gross saving:

```
requests            11
messages            69 total, 7 compressed
tokens              169089 in, 62818 out
saved gross         106271
retrieved back      542 over 6 retrievals (6 hits, 100.0% hit rate)
saved net           105729
```

`requests 0` means codex never reached the proxy — the base URL is not being
honoured, or a WebSocket carried the traffic.

If requests arrive but nothing compresses, check the recorded family:

```bash
sqlite3 ./data/stats.db "select api_family, count(*) from proxy_requests group by 1;"
```

`passthrough` on a model request means the mount explicitly disables parsing,
or the path is one `detect_family` does not know. `openai_responses` is correct.

To confirm the fragments are genuinely recoverable rather than merely elided:

```bash
sqlite3 ./data/stats.db "select refs_emitted, ref_bytes_emitted, outcome from optimization_events;"
```

`refs_emitted = 0` on a row that compressed means the elision left no handle —
see Tuning.

## Tuning

**`engine.max_ref_bytes` — raise it.** This is the one default worth changing
before you start. In `recoverable` mode a fragment larger than the cap is elided
with **no handle**, and the marker is indistinguishable from a recoverable one
apart from the missing `ref:`. Agent tool output crosses 64 KiB routinely.
Measured on a 77 KB log:

```
max_ref_bytes = 65536   →  [... 880 log lines omitted; 0 ERROR, 9 WARN]
max_ref_bytes = 1048576 →  [... 880 log lines omitted [ref:k_jC66WigedLte]; 0 ERROR, 9 WARN]
```

Both compress 97.7%. Only the second can be asked for back. The payload that
compresses best is the one most likely to lose its handle.

**`proxy.retrieve_max_rounds`** (default 3) and
**`proxy.retrieve_max_tokens_per_request`** (default 16384) bound what the
retrieve loop may re-inject. The defaults were comfortable in practice —
observed retrievals came back narrowed to 86–159 characters each.

Comfortable on the HTTP path, that is. A relayed websocket session spends the
token budget per *connection* rather than per request, and a measured sweep
restored 7 fragments and had 11 refused on one connection. `kompressor stats
retrievals` reports those as `withheld`, separately from the hit rate: a rising
count there is this number binding, and nothing else reports it.

**Leave `proxy.stream_buffer_timeout` at zero.** Flushing the relay's buffer on
a timer hands a `optscale_kompressor_retrieve` call to codex, which has never heard of
that tool.

## What to expect

> **Two different measurements live below.** The table is the engine's
> behaviour on codex-*shaped* payloads, driven by `make harbor-lab` against a
> recording upstream. Seven measured sessions against the real client and a
> real ChatGPT plan are in
> [Real sessions, by payload type](#real-sessions-by-payload-type) underneath
> it, and those are the ones to plan against.

Measured on codex 0.155.1 against a 350 KB redundant build log, gpt-6-astra,
streamed Responses, recoverable mode.

| | |
|---|---|
| Large tool result, per payload | 69,190 → 31,101 chars (−55%), 24,625 → 7,390 tokens (−70%) |
| Homogeneous log, per payload | 77,124 → 1,792 chars (−97.7%) |
| Aggregate over a session | 169,089 → 62,818 tokens (−62.9%) |
| End-to-end billed tokens, same task | 27,921 → 22,227 and 27,925 → 22,476 (**≈ −20%**) |
| Wall clock, same task | 6.1 s → 10.9 s, 7.8 s → 15.8 s (**≈ 2×**) |
| Engine throughput | 0.15–0.32 ms/KB |
| Accuracy | 4 of 4 tasks answered correctly, including needles recovered from elided regions |

The gap between −70% on a payload and −20% billed is structural, not a defect:
codex's prompt is mostly `developer`-role instructions and the model's own
turns, all of which are protected by design. Only tool results are eligible.

The wall-clock cost is the retrieve loop — two extra provider round trips when
the model asks for something back. Prompt caching survives it: cache reads of
23,170 and 23,113 tokens were observed on the retrieve rounds, because the
content-addressed markers keep the cached prefix byte-identical.

### Real sessions, by payload type

Seven `codex exec` sessions on a ChatGPT plan, 2026-09-22, recoverable mode,
OptScale Kompressor 0.3.0. Each asked codex to `cat` a large file and quote its last
line, so the whole payload had to cross the wire. `in`/`out` are the characters
the engine was offered and what it forwarded, over the whole session.

| payload | offered | forwarded | removed | refs |
|---|---|---|---|---|
| search (`grep -rn`) | 92,342 | 20,016 | **78.3%** | 15 |
| tabular (`ps aux`) | 49,825 | 13,018 | **73.9%** | 1 |
| listing (`find`) | 92,492 | 26,164 | **71.7%** | 8 |
| log | 52,131 | 17,190 | **67.0%** | 1 |
| json | 52,132 | 26,097 | **49.9%** | 3 |
| config (`Cargo.lock`) | 52,134 | 34,353 | **34.1%** | 0 |
| build (`cargo build -v`) | 29,042 | 24,291 | **16.4%** | 2 |
| **all seven** | **420,098** | **161,129** | **61.6%** | |

Re-run on 2026-09-22 against a reset statistics database; the per-type figures
reproduced to the decimal except `config`, where codex chose to read less of
the file. Six of seven answered a verbatim question about their payload
correctly; the seventh is below.

Read it as a range, not a number: what a session saves is decided by what it
reads. A session spent in logs and listings saves two thirds or better; one
spent in build output or source saves very little.

Two things the table does not show. **Codex truncates** large tool output
before the proxy sees it — 232 KB of grep output arrived as 40 KB — keeping the
head and the tail. And **the payload size is the model's own choice**: the tool
call carries a `max_output_tokens` the model picks per call, and it was 2,000
on some calls and 20,000 on others for identically shaped prompts. Neither is
something a proxy can influence.

#### The one wrong answer

The `search` session was asked for the payload's last line and named a
different real line from the same corpus. It reproduced exactly on a second
run, so it is worth being precise — and the precise answer is not the obvious
one. Nothing was elided, nothing was lost, and the retrieve loop worked.

**The search compressor re-orders.** It groups hits by the file they came from
and *ranks the files*, which is stated in `engine::search`'s own module doc and
is the whole basis of bounded sampling. So the compressed view is deliberately
not in input order. Measured on the payload codex sent:

| | original | compressed view |
|---|---|---|
| position of `compress.rs:148` | line 460 of 460 — the last line | line 41 of 52 |
| last content line | `compress.rs:148` | `configfile.rs:627`, then two markers |

The true last line **was in the view the whole time**, visible, un-elided. It
simply was not last any more. The model, asked for "the last line", took the
last thing on the page — the final omission marker — retrieved its handle
(`k_J28GggsWOYUa`, served, 983 characters, a hit) and answered with the last
line of *that* fragment.

So this is not a retrieval failure and not data loss. It is a question about
**position** asked of a view that does not preserve position, and the marker
placed last does not mean "what was last". Everything else the seven sessions
asked was answered correctly, including on payload types whose compressors do
preserve order.

Worth knowing if you are building on this: `search` output is ranked, not
sequential. Ask it what matched, not what came last.

## Known limits## Known limits

- **A subscription stream arrives with no `Content-Type`.** chatgpt.com sends
  the Responses SSE stream without one. Until 0.3.0 the proxy required the
  header to recognise a stream, so every subscription response took the
  buffered path: the relay never ran, the model's `kompressor_retrieve` call
  reached codex instead of being executed here, codex rejected it with
  `unsupported call: kompressor_retrieve`, and the model answered from what was
  left. Measured on one prompt, same payload and client, only the mode
  differing — `lossless` answered 155, `recoverable` answered 778. An absent
  content type is now read as the stream the request asked for, and the same
  prompt answers 155 in both modes with one retrieve round executed
  server-side. A content type that *is* present is still believed over what we
  asked for.
- **Source code is mostly declined.** Measured on a 40 KB Rust tool result:
  0.0% saved, detected as `source_code` with the language read as C. The
  code-aware compressor is feature-gated and conservative; a codex session
  spent reading source will see much less than one spent reading logs.
- **Codex truncates large tool output before the proxy sees it.** A 349,718-byte
  `cat` reached OptScale Kompressor as 69,138 bytes. That loss happens upstream of
  compression and is not recoverable through a handle, because the bytes never
  arrived. It also caps how much there is to save.
- **`x-kompressor-retrieve-rounds` is always `0` on a streamed response.**
  Response headers necessarily precede the loop. The real count is in
  `proxy_requests`.
- **A ranged retrieval that matches nothing now says how it missed.** It is
  still a hit — the handle resolved — so `found` stays `true` and the content is
  empty, but the payload carries a `hint` naming the variant that missed and
  what to try instead (a line window gets the fragment's real line count; a
  pattern is told the match is literal, not a regular expression). Before that,
  a model could spend several of its retrieval rounds on empty answers with
  nothing to learn from: measured at two of four rounds on one opencode run,
  which then hit the round cap and lost an answer.
- **A subscription session is relayed, and a relayed session reports no
  usage.** The built-in provider's transport preference cannot be disabled from
  a top-level key, so every subscription session opens a WebSocket. The proxy
  relays it — `proxy.websocket_relay` ships on — and compresses the
  `response.create` frames, which also ends the roughly 13 seconds the first
  request used to spend on five refused handshakes before falling back to
  HTTPS. What it costs is the accounting: the relay never reads a
  `response.completed`, so that session's token counts are filed null and no
  cost report can price it. `KOMPRESSOR__PROXY__WEBSOCKET_RELAY=false` brings
  back the refusal, the fallback and the numbers. An API-key session goes
  through the provider block, where `supports_websockets = false` keeps it on
  HTTP either way.
- **Fragment storage is world-readable.** `refs.db` holds elided tool output
  verbatim at the default umask. Put `refs.path` somewhere with restricted
  permissions on a shared machine.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `kompressor stats summary` shows `requests 0` | codex never reached the proxy | Check `model_provider` names your provider, and that `openai_base_url` is set for a subscription login |
| Thirteen-second stall on the first request, then it works | WebSocket handshake failing over to HTTPS | `supports_websockets = false` on the provider; not available for the subscription path, which uses the built-in provider |
| Requests arrive, `saved_tokens: 0`, family `passthrough` | Parsing explicitly disabled or endpoint unrecognised | Remove `api_family = "passthrough"` or check the forwarded path |
| `404` from the proxy on non-model calls | Only the codex mount is configured | Add the `/backend-api` passthrough mount |
| `Error loading config.toml: model_providers contains reserved built-in provider IDs` | Provider named `openai` | Rename it, e.g. `kproxy` |
| Large payload remains uncompressed with `refs_emitted = 0` | Its elided fragment exceeds `max_ref_bytes` | Raise `engine.max_ref_bytes` if the store can hold it |
| `401` through the proxy, works directly | Credential not reaching the upstream | `forward_auth = true` on the mount |
| `Missing environment variable: OPENAI_API_KEY` under an API-key login | The provider block has `env_key`, and the key is stored in `auth.json` | `requires_openai_auth = true` in place of `env_key`; `wrap` writes that itself |
| `codex` will not start: duplicate key | `wrap` refused, or a block was left behind | `kompressor unwrap codex`, then remove your own `model_provider` / `openai_base_url` if you set one |
| Blocks left in `config.toml` after a crash | The session could not clean up | `kompressor unwrap codex` |
