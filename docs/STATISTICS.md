# Statistics: dashboards and the JSON API

What Kompressor records about its own work, and where to read it. For which
number to trust, start with
[Reading the savings honestly](../README.md#reading-the-savings-honestly).

## Dashboards

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
  reasoning elision, and tool deferral, which is not built — with whether it
  is on now, a verdict (turn on, not worth it, not enough data), the value to
  set, the figure the verdict rests on beside the bar it was held against, a
  ceiling on the saving, and the reason in a sentence. Under it, the figures
  behind each row and what the settings that are on have actually done. The
  bars are the design's proposals, printed with each verdict, not measured
  optima; three of the figures (what compaction would remove, what a
  recompaction would rewrite, whether a client names its own cache key) are
  measured on every request without acting on them.

## Statistics JSON API for automation

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
| `GET /v1/stats/analysis/compare?from=&to=` | historical comparison; `compare_from`/`compare_to`, `model`, `mode`, `surface`, `experiment_id`, `run_id` |
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
