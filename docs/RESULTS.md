# Results

The full benchmark results behind the summary in the
[README](../README.md#results): method, per-cohort and per-content-type
breakdowns, and how to read them.

Every figure below is the difference between real provider bills, with the
prompt-cache discount included. Self-reported savings, which price removed tokens
at the uncached list rate, are not used. Each feature has its own section:

- [Codex on GPT-5.5](#results-codex): cache-safe compression of tool output
- [Claude Code on Opus 5.5](#results-claude-code): the cache is the bill

<a id="results-codex"></a>
### Codex on GPT-5.5

**Feature:** [Cache-safe compression of tool output](../README.md#feature-cache-safe-compression) ·
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
through three engines at default settings: optscale-kompressor 0.13 (the pre-1.0 build the replay was run on), Headroom
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

**Feature:** [On Claude Code, the cache is the bill](../README.md#feature-claude-code-cache) ·
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
