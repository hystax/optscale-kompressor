# OptScale Kompressor — design

Deterministic prompt compression for agent traffic, as one Rust binary: a
compression engine, an HTTP API, an MCP server and an LLM reverse proxy, in any
combination, over one implementation of the algorithm.

This document is the design and the implementation spec. It is written to be
read in three ways: §1–§3 for what the system is and how it is put together,
§4–§7 for the engine (the part that actually compresses, and the only part with
no I/O), and §8–§20 for the surfaces, storage, configuration and operations
around it.

Numbers quoted as **measured** come from replaying recorded corpora of real
agent tool output; see §18. Where a constant was chosen rather than measured,
the text says so.

---

## Table of contents

1. Goals, non-goals, principles
2. Product surface: work modes and compression modes
3. Architecture and crate layout
4. `kompressor-engine`: types, traits, pipeline
5. Engine algorithms in detail
6. Content detection: the ordered chain and every rule in it
7. The compressors
8. `kompressor-store`: the CCR fragment store
9. `kompressor-stats`: statistics in SQLite
10. `kompressor-server`: HTTP compression API
11. `kompressor-mcp`: MCP server
12. `kompressor-proxy`: LLM proxy mode
13. Configuration
14. CLI
15. Observability
16. Security and multi-tenancy
17. Performance and concurrency model
18. Testing and benchmarking
19. Status
20. Open questions

**Appendices** — A. Marker grammar · B. Full SQLite DDL · C. Configuration reference · D. Glossary · E. Data model reference

---

## 1. Goals, non-goals, principles

### 1.1 Goals

1. **One static binary** (`kompressor`) that runs as a library/CLI, an HTTP compression API, an MCP server, and an LLM reverse proxy — any subset, in one process or several.
2. **Behaviour locked by measurement, not by trust.** Every constant that decides what compresses is justified by a corpus replay, and the whole engine is pinned to recorded vectors so that a change in *which payloads compress and to what* cannot arrive as a side effect of tuning something else (§18).
3. **All three compression modes**, `recoverable` as the default everywhere (engine default, config default, MCP tool default, proxy default).
4. **SQLite for statistics** — optimization, retrieval, protected-ref and proxy-request events, with the gross/net economy computable by SQL and served by the app itself.
5. **No infrastructure dependencies by default**: SQLite for fragments and statistics, memory for tests, Redis behind a feature flag for multi-instance deployments.
6. **Prompt-cache safety as a first-class constraint**: byte-stable output, frozen-prefix and `cache_control` awareness in the proxy, no query-dependence.
7. **Operable**: Prometheus metrics, structured logs, health/readiness, a `bench` subcommand with the corpus harness, a `doctor`-style config check.

### 1.2 Non-goals (v1)

- ML or model-based compression, prose summarisation, HTML extraction, image handling. Everything here is deterministic and rule-based by construction.
- A dashboard UI (the stats API and SQLite file are the interface; Grafana over Prometheus or SQLite is the expected consumer).
- Gemini/Bedrock/Vertex wire formats in the proxy (designed for as extension points; OpenAI Chat, OpenAI Responses and Anthropic Messages ship).
- Coupling to any particular gateway, identity service or analytics warehouse. Tenancy is a string resolved from a credential (§16), and statistics land in local SQLite; exporting them elsewhere is an adapter, not a core concern.

### 1.3 Principles

- **Fail open.** Any error in compression, storage or stats degrades to "forward the original". The proxy must never turn a provider 200 into a 5xx because of us.
- **Never larger, always accounted, always deterministic.** The three engine invariants are enforced centrally in the pipeline, not per compressor, so none of them can opt out.
- **Bytes outside a changed span are never touched.** Offset-based rewriting everywhere (segmenter, code, proxy body edits).
- **No network in the engine.** Grammars, tokenizer weights and rules are compiled in.
- **Measured constants stay measured.** Every constant carried over keeps its provenance comment; changing one requires a corpus replay diff.
- **Tenant scoping by construction** (refs keyed by tenant), never by filtering.

## 2. Product surface: work modes and compression modes

### 2.1 Work modes (surfaces)

| Surface | Subcommand | What it does | Who uses it |
|---|---|---|---|
| **Library** | (crate `kompressor-engine`) | `compress_content`, `compress_messages`, `detect`, `retrieve` over any `RefStore` | embedding in other Rust services |
| **CLI** | `kompressor compress`, `retrieve`, `detect`, `stats`, `bench`, `check-config` | one-shot compression of files/stdin, corpus replay, stats queries | developers, CI |
| **Compression API** | `kompressor serve --api` | HTTP `/v1/compress`, `/v1/compress/raw`, `/v1/retrieve`, `/v1/stats/*`, `/healthz`, `/readyz`, `/metrics` | agents/harnesses that orchestrate compression explicitly; other proxies |
| **MCP server** | `kompressor serve --mcp` (streamable HTTP) or `kompressor mcp serve` (stdio) | tools `optscale_kompressor_retrieve`, `optscale_kompressor_compress`, `optscale_kompressor_compress_raw`, `optscale_kompressor_stats` | LLM gateways and agent runtimes |
| **LLM proxy** | `kompressor serve --proxy` | transparent reverse proxy for OpenAI Chat Completions, OpenAI Responses, Anthropic Messages: compresses in-flight, injects and executes the retrieve tool, records real usage | any client, by changing `base_url` |
| **Agent wrap** | `kompressor wrap AGENT` | starts the proxy, points a coding agent at it, runs it, and cleans up; sessions share one proxy | anyone who wants compression without configuring anything (§14.1) |

`kompressor serve` with no flags runs all three surfaces on one listener (paths are disjoint); `--api/--mcp/--proxy` select subsets; separate `--listen` per surface is supported so the proxy can be exposed and the API kept internal.

### 2.2 Compression modes

`aggressive | lossless | recoverable`, as defined above. Precedence (highest first): per-request `options.mode` (API/MCP) or `X-Kompressor-Mode` header (proxy) → per-tenant config override → `[engine] mode` in config → built-in default **`recoverable`**. A fourth value `off` is accepted at the header/tenant level (not an engine mode): pass-through with a stats row of `mode='off'`.

Recoverable is only useful when something can retrieve. In the proxy the app *is* that something (tool injection + server-side execution, §12.6), so the default is safe there. For the bare API a caller who cannot retrieve should pass `aggressive`; the MCP tool description says so.

## 3. Architecture and crate layout

### 3.1 Runtime architecture

![runtime architecture](charts/runtime.drawio.png)

The same thing in text, which is the version that survives a diff:


```
                         ┌──────────────────── OptScale Kompressor (one process) ─────────────────────────────┐
  clients                │                                                                                     │
  ────────               │  axum router (tower layers: request-id, tracing, body limit, timeout, auth)          │
  OpenAI/Anthropic SDKs ─┼─▶ /v1/chat/completions  /v1/responses  /v1/messages  ──▶ proxy handlers ──┐         │
  agents/harnesses ──────┼─▶ /v1/compress  /v1/compress/raw  /v1/retrieve  /v1/stats/*  ──▶ api handlers ─┤  │
  MCP clients ───────────┼─▶ /mcp  (streamable HTTP)                                       ──▶ mcp tools ─┤  │
                         │                                                                        ▼          │
                         │                                            ┌──────────────── Core (Arc<AppCore>) ──┐│
                         │                                            │ Engine (pure, Send+Sync, stateless)     ││
                         │                                            │ RefStore (trait) ── SqliteRefStore      ││
                         │                                            │                     MemoryRefStore      ││
                         │                                            │                     RedisRefStore (feat)││
                         │                                            │ Stats (mpsc → writer thread → SQLite)   ││
                         │                                            │ Tenants/keys · Config · Metrics         ││
                         │                                            └─────────────────────────────────────────┘│
                         │        proxy ──reqwest (rustls, h2, streaming)──▶ upstream providers                 │
                         └───────────────────────────────────────────────────────────────────────────────────────┘
                                   SQLite files: refs.db (WAL)   stats.db (WAL)      Prometheus /metrics
```

#### What each component is

| Component | Crate | What it is, and what it is responsible for |
|---|---|---|
| **axum router** | `kompressor-server` | The single listener every surface shares. Applies tracing, a body limit and a timeout as tower layers, and request identity, credential resolution and metrics as middleware. Composition order is load-bearing in exactly one place: the proxy is merged last, because it has a catch-all fallback and would otherwise swallow `/v1/compress`. |
| **proxy handlers** | `kompressor-proxy` | The reverse proxy. Walks a request for compressible text, compresses under a deadline, writes only changed slots back, offers the model a retrieve tool and **executes that tool itself** — which is what makes `recoverable` safe for a client that has never heard of this service. Owns the SSE relay state machine (§12.9). |
| **api handlers** | `kompressor-server` | Compression as an explicit call, for a harness that would rather orchestrate it than have it happen in flight. Also the operational endpoints: liveness, readiness, Prometheus, and the statistics reports. |
| **mcp tools** | `kompressor-mcp` | The same operations as MCP tools, over stateless streamable HTTP, for an agent runtime that wants `optscale_kompressor_retrieve` directly rather than through a proxy. |
| **Core** (`Arc<AppCore>`) | `kompressor-core` | The operations themselves — compress, compress_raw, retrieve — plus configuration and credential resolution. Every surface above holds *the same* `Arc`, which is why the three cannot answer the same question differently: they are translation, not logic. |
| **Engine** | `kompressor-engine` | Detection, the compressors, arbitration and the invariants. Free functions over a payload: no state, no I/O, no async. That is what makes it testable without a runtime and replayable against a corpus — and why handlers hand it to `spawn_blocking` rather than running it on an executor thread. The largest crate by far (~12.6k lines). |
| **RefStore** (trait) | `kompressor-store` | Retention for the fragments a `recoverable` elision put behind a handle, keyed `(tenant, handle)` so a leaked handle resolves to nothing elsewhere. `SqliteRefStore` is the default and `MemoryRefStore` is for tests; a `rusqlite::Connection` is `!Sync`, so a dedicated thread owns it behind a channel, fronted by a write-through cache. TTL is an idle window refreshed on read, capped by an absolute lifetime. |
| **Stats** | `kompressor-stats` | Append-only events written entirely off the request path: a bounded channel into a writer thread that batches into transactions. A full queue **drops** rather than blocking, because losing a row is better than making a compression wait for a disk. Owns the gross-versus-net economy queries. |
| **Authenticator · Config** | `kompressor-core` | The tenant comes from the credential and never from an argument, so a caller cannot name someone else's fragments. Config is layered file → environment → flags and validated on load. |
| **reqwest** | (dependency) | The upstream client: rustls, HTTP/2, streaming bodies, redirects followed by us rather than silently, because the proxy may need to re-send a body for a retrieval round. |
| **refs.db · stats.db** | — | Two SQLite files in WAL mode. Separate because they have nothing in common: one is hot, small and read on the request path; the other is append-only, grows without bound and is read by humans. |
| **Upstream providers** | external | OpenAI, Anthropic, or anything speaking their wire formats — including something local. The longest matching path prefix wins; an unmatched path is a 404 rather than a guess. |

- The engine is CPU-bound and synchronous; handlers run it on `tokio::task::spawn_blocking` (or a dedicated rayon pool, §17).
- Stores are behind traits; SQLite connections are owned by dedicated threads (rusqlite `Connection` is `!Sync`), fronted by channels.
- Stats are fire-and-forget through a bounded channel; the writer batches into transactions.

![kompressor-core](charts/core.drawio.png)

`kompressor-core` is the box in the middle of that picture, opened up: the two
operations the three surfaces share, and the credential resolution that runs
before either of them because the tenant scopes everything durable.

### 3.2 Cargo workspace

Ten crates. The split is not cosmetic: each boundary is somewhere a dependency
must *not* cross, and the table's `async` column is the one that matters most —
everything above the line is testable without a runtime.

| Crate | Depends on | async | What lives there |
|---|---|---|---|
| `kompressor-engine` | — | no | Detection, every compressor, arbitration, the invariants, markers and handles. No I/O and no async, by construction: it is free functions over a payload, which is what lets it be replayed against a corpus. ~12.6k lines. |
| `kompressor-store` | engine | no | The `RefStore` trait and its SQLite and memory backends, the write-through cache, and the idle-window TTL. Synchronous API — the async wrapping happens in the surfaces. |
| `kompressor-stats` | engine | no | Event types, the SQLite writer thread, the retention pruning and the gross-versus-net economy queries. |
| `kompressor-wire` | engine | no | The three provider wire formats — OpenAI Chat, OpenAI Responses, Anthropic Messages — and the walkers that find compressible slots, compute the frozen prefix and write only what changed back. |
| `kompressor-core` | engine, store, stats | no | The operations themselves (`compress`, `compress_raw`, `retrieve`), configuration, and credential-to-tenant resolution. The thing all three surfaces share, so they cannot drift apart. |
| `kompressor-server` | core, engine, store, stats | yes | The axum app: the compression API, the shared state, the concurrency bound in front of a synchronous engine, the error envelope, request identity and the Prometheus metrics. |
| `kompressor-mcp` | core, engine, store, stats | yes | The MCP tools over `rmcp`, streamable HTTP, stateless. |
| `kompressor-proxy` | core, engine, store, stats, wire | yes | The reverse proxy: routing, the compression path, tool injection, the server-side retrieve loop and the SSE relay state machine. |
| `kompressor-bench` | engine | no | The corpus format, replay scoring, baseline comparison and the invariant checks. |
| `kompressor-cli` | all of the above | yes | The `kompressor` binary: `wrap`, `serve`, `compress`, `retrieve`, `stats`, `bench`. |

**Dependency direction.** `engine` is the root and depends on nothing in the
workspace; `store`, `stats`, `wire` and `bench` each depend only on it; `core`
sits on `engine + store + stats`; and the three surfaces — `server`, `mcp`,
`proxy` — each depend on `core`, **not on each other and not on `server`**. That
last point is what keeps them interchangeable: a surface is a translation layer
over `core`, so adding a fourth costs nothing the others have to know about.

`engine`'s own dependencies are `serde`, `serde_json`, `regex`, `memchr`,
`blake2`, `base64`, `flate2`, `csv`, `once_cell`, `tracing`, and — behind the
default `code-aware` feature — `tree-sitter` with nine grammar crates. There is
no `tokio` in `engine`, `store`, `stats`, `wire`, `core` or `bench`.

Outside `crates/`:

```
Cargo.toml        workspace: shared dependency versions and lints
fixtures/         corpus.jsonl.gz · baseline.json · golden.json.gz
docs/             this document, QUICKSTART.md, BENCHMARK.md, the diagrams
scripts/          grouped by job: build/ · db/ · docs/ · harbor/ · service/ ·
                  kompressor-codex-sweep/ · claude-sub/ · smoke.sh
packaging/        what the OS packages install: systemd/ and debian/ for the
                  .deb and .rpm, macos/ for the .pkg's LaunchAgent
tools/            export_golden.py — regenerates the recorded vectors
                  codex-limits.sh  — what the codex plan has left
Makefile          every target, self-documenting via `make`
```

### 3.3 Key dependencies (pinned in the workspace)

| Concern | Crate | Notes |
|---|---|---|
| async runtime / HTTP | `tokio`, `axum 0.8`, `tower`, `tower-http` (trace, request-id, limit, timeout, cors) | one router per surface, merged |
| upstream client | `reqwest 0.12` (rustls, http2, stream) | streaming bodies, connection pool per upstream |
| JSON | `serde`, `serde_json` with **`preserve_order`** and **`arbitrary_precision`** | key order and numeric literals round-trip exactly (§5.3) |
| regex | `regex` (+ `regex-automata` for line-anchored multi-pattern sets) | compile once in `LazyLock`; no look-around available (§5.1) |
| hashing | `blake2` (`Blake2bVar` 9 bytes; `Blake2b` 8 bytes for simhash), `base64` (URL_SAFE no-pad — 9 bytes → 12 chars exactly) | handle derivation (§4.4) |
| deflate | `flate2` with the `zlib-rs` backend | the `adaptive_k` compressibility check (§5.2), and gzip/deflate request bodies (§12.1) |
| zstd | `zstd` | request bodies: codex sends every one of them zstd-encoded (§12.1) |
| grammars | `tree-sitter 0.25`, `tree-sitter-{python,javascript,typescript,go,rust,java,c-sharp,c,cpp}` | statically linked; feature `code-aware` (default on) |
| SQLite | `rusqlite` (`bundled`, `modern_sqlite`) | one writer connection per DB on its own thread; readers via a small pool |
| Redis (optional) | — | **designed, not built.** `RefsBackend` is `Sqlite` or `Memory`; there is no `redis` dependency and no `redis` feature. See the README's *Not yet built* |
| MCP | `rmcp` (official Rust MCP SDK: server, `streamable-http-server`) | stateless streamable HTTP |
| CLI/config | `clap` (derive, env), `toml`, `humantime-serde` | layered config (§13). Neither `figment` nor `config` is used: the layering is hand-rolled, which is what lets `deny_unknown_fields` reject a typo instead of ignoring it |
| observability | `tracing`, `tracing-subscriber` (json/env-filter), `metrics` + `metrics-exporter-prometheus` | |
| tests | `wiremock` (upstream mock) | `proptest`, `insta`, `criterion` and `cargo-fuzz` are **not** in the tree. Golden vectors are a hand-rolled snapshot (`fixtures/golden.json.gz`, §18) and the benchmark is `kompressor bench replay` against a recorded baseline, not criterion |

Edition 2021. MSRV is **1.90**, declared in `Cargo.toml` and gated by a CI job that builds on exactly that toolchain — the floor is set by the dependency graph (`tree-sitter-language`), not by anything this workspace writes, and `--no-default-features` does not lower it because cargo resolves `rust-version` across the whole locked graph. `#![forbid(unsafe_code)]` is in nine of the ten crates; `kompressor-cli` is the exception.

## 4. `kompressor-engine`: types, traits, pipeline

![engine pipeline](charts/engine.drawio.png)

One payload's path through the crate, in one picture: the two arbitrated
detours, the routing decision, and the guards every candidate has to pass
before it may ship. The sections below are that diagram in detail.

### 4.1 Core types

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode { Aggressive, Lossless, Recoverable }
impl Default for Mode { fn default() -> Self { Mode::Recoverable } }   // the shipped default is the recommended one

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContentType {           // the detector's vocabulary; ten outcomes
    JsonArray, SourceCode, SearchResults, BuildOutput, GitDiff, Html, Tabular, Listing, Config, PlainText,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Strategy { SmartCrusher, Search, Log, Diff, CodeAware, Tabular, Listing, Config, Passthrough }

/// Analytics vocabulary — what `by_type`/`content_types` report. Mixed unfolds into these.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Json, Log, Text, Search, Diff, Tabular, Listing, SourceCode, Config, Html, Mixed, Reformat, Factored, StaleRead, CrossTurn }
// analytics mapping: JsonArray→Json, BuildOutput→Log, PlainText→Text, others by name

#[derive(Clone, Debug)]
pub struct Detection { pub content_type: ContentType, pub confidence: f32, pub meta: DetectionMeta }
#[derive(Clone, Debug, Default)]
pub struct DetectionMeta {
    pub item_count: Option<usize>, pub concatenated_json: bool, pub sample_truncated: bool,
    pub diagnostics: bool, pub untimestamped: bool, pub uniform_stream: bool,
    pub language: Option<Language>, pub flavor: Option<ConfigFlavor>, pub skipped_size: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Context<'a> {         // what a compressor may consult, and mostly does not
    pub role: &'a str, pub message_index: Option<usize>, pub preceding_user_text: &'a str,
    pub source_tool: Option<&'a str>, pub file_path: Option<&'a str>,
}
```

### 4.2 `EngineOptions` (every knob, at its default — see §13 for the file form)

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineOptions {
    pub mode: Mode,                                 // recoverable
    pub min_compress_chars: usize,                  // 512
    pub max_array_items: usize,                     // 10
    pub max_field_chars: usize,                     // 256
    pub keep_tail_lines: usize,                     // 20
    pub ref_ttl_seconds: u64,                       // 3600  (idle window, see §8)
    pub ref_max_lifetime_multiplier: u32,           // 8     (absolute cap on the idle window)
    pub max_ref_bytes: usize,                       // 1048576 (UTF-8 bytes); recoverable refuses an elision over it
    pub enable_smart_crusher: bool,                 // true
    pub enable_search_compressor: bool,             // true
    pub enable_log_compressor: bool,                // true
    pub enable_diff_compressor: bool,               // true
    pub enable_tabular_compressor: bool,            // true
    pub enable_listing_compressor: bool,            // true
    pub enable_code_aware: bool,                    // true
    pub enable_mixed_content: bool,                 // true
    pub enable_config_compressor: bool,             // true
    pub enable_nested_routing: bool,                // true
    pub enable_redraw_fold: bool,                   // true  (arbitrated; needs a collector)
    pub enable_build_templates: bool,               // true  (build output folds by name too, §7.4)
    pub fold_handles_identical_repeats: bool,       // false (a `[×N]` over identical lines needs no handle, §7.4)
    pub enable_html_compressor: bool,               // true  (HTML folds through the log route, §7.10a)
    pub enable_normalised_text_fold: bool,          // true  (a plain-text fold may normalise values, §7.10c)
    pub enable_tabular_ratio: bool,                 // true  (a table must be mostly rows, §6)
    pub enable_code_budget: bool,                   // true  (a second pass when no body cleared the floor, §7.9)
    pub code_budget_min_body_lines: usize,          // 6
    pub enable_gutter_aware_detection: bool,        // true  (classify by what is under the numbers, §6)
    pub enable_lossy_key_factoring: bool,           // true  (reversible, so safe in any mode)
    pub enable_stale_read: bool,                    // true
    pub enable_cross_turn_dedup: bool,              // true  (fold what an earlier message showed, §5.12a)
    pub search_max_files: usize,                    // 8
    pub diff_max_files: usize,                      // 8
    pub code_max_body_lines: usize,                 // 24
    pub min_nested_chars: usize,                    // 512
    pub max_nested_depth: usize,                    // 2
    pub protected_roles: Vec<String>,               // ["assistant","system","developer"]
    pub token_guard: TokenGuard,                    // Chars (default) | Tokens (estimator) — see §5.13
}
```

`EngineOptions::merged(&self, overrides: &EngineOptionsPatch) -> EngineOptions` where `EngineOptionsPatch` is the all-`Option` mirror used by the API (`options` body field) and by per-tenant config. Validation ranges are checked on load ('s `EngineOptionsIn` (`max_field_chars ≥ 16`, …).

### 4.3 Text length semantics

Every threshold in the engine is measured in **code points**, except `max_ref_bytes`, which is UTF-8 bytes because it bounds what is written to the store. Rust strings are bytes, so this is a decision that has to be made explicitly and in one place — otherwise a payload's eligibility would depend on how much non-ASCII it holds. The engine defines:

```rust
pub trait TextLen { fn chars_len(&self) -> usize; }      // code points, cached where hot (payload struct)
```

and uses `chars_len()` for every threshold over text (size gate, clip limit, never-larger comparisons, 2% reformat floor, and the 128-character redraw floor — named in bytes but counted in characters, like the rest`str`, i.e. chars; we keep chars). Fragment caps and stats byte columns use UTF-8 bytes. A `Payload` wrapper computes `chars_len` once per message (O(n), cheap next to regex scans) and caches line splits.

Line splitting recognises the full Unicode separator set — `\n`, `\r\n`, `\r`, `\v`, `\f`, `\x1c`–`\x1e`, `\x85`, `\u{2028}`, `\u{2029}` — not just `\n`, so that a payload using an exotic separator cannot smuggle content past a line-oriented compressor. The engine ships `split_lines(&str) -> Vec<&str>` (and `split_lines_keepends`) for exactly that set; `str::lines()` is never used in a compressor. Whitespace trimming uses `char::is_whitespace`, i.e. Unicode `White_Space`.

### 4.4 `RefCollector` and markers

```rust
pub struct RefCollector { max_ref_bytes: usize, pairs: Vec<(RefId, String)>, seen: HashSet<RefId>, oversized_attempts: usize }
pub struct RefId(pub [u8; 12]);           // "k_" + 12 url-safe chars, stored without the prefix, Display adds it
impl RefCollector {
    pub fn new(max_ref_bytes: usize) -> Self;
    pub fn stash(&mut self, fragment: &str) -> Option<RefId>;   // None over cap; dedup by id
    pub fn mark(&self) -> usize;  pub fn rollback(&mut self, mark: usize);
    pub fn scratch(&self) -> RefCollector;                       // same cap, empty
    pub fn adopt(&mut self, other: RefCollector);                // replay by fragment (content-addressed ⇒ exact)
    pub fn pairs(&self) -> &[(RefId, String)];
}
pub fn ref_for(fragment: &str) -> RefId;   // BLAKE2b-9 → base64url (no pad) → `k_` + 12 chars
pub fn refs_in(text: &str) -> Vec<RefId>;  // memchr "ref:" gate, then manual scan: "k_" + 12 [A-Za-z0-9_-] + non-word boundary
```

`markers` module: `has_marker`, `whitespace_only`, `omission_marker(kind, count, fragment, refs, suffix)`, `redraw_marker`, `code_marker`, `clip_marker`, `fold_marker`, `array_marker` — the only place marker text is spelled; Appendix B is generated from it by a test.

### 4.5 Compressor trait and registry

```rust
pub trait Compressor: Send + Sync {
    fn strategy(&self) -> Strategy;
    /// Returns `None` when it declines (caller keeps the input untouched). Must stash only what it elides,
    /// must not stash when `refs` is None, must be deterministic, must not read `ctx.preceding_user_text`
    /// in a way that changes bytes (query-stability invariant).
    fn compress(&self, input: &Payload, opts: &EngineOptions, ctx: &Context, refs: Option<&mut RefCollector>,
                det: &Detection) -> Option<String>;
}
```

Concrete: `SmartCrusher`, `LogCompressor { normalize: bool }`, `SearchCompressor`, `DiffCompressor`, `TabularCompressor`, `ListingCompressor`, `CodeCompressor`, `ConfigCompressor`. `Engine` holds them plus the detector and the segmenter; it is `Send + Sync` and cheap to clone (`Arc` inside).

### 4.6 Public engine API

```rust
pub struct Engine { /* Arc<Inner> */ }
impl Engine {
    pub fn new(opts: EngineOptions) -> Engine;
    pub fn options(&self) -> &EngineOptions;
    pub fn detect(&self, text: &str, role: &str) -> Detection;                     // size-gated
    pub fn compress_content(&self, text: &str, role: &str, forced: Option<ForcedType>,
                            refs: Option<&mut RefCollector>, ctx: &Context, applied: &mut Vec<Kind>)
        -> (Cow<str>, Option<Kind>);                                              // §5.14 pipeline
    pub fn compress_messages(&self, msgs: &mut [Message], req: &CompressRequestOpts) -> CompressStats; // §5.15 route logic
    pub fn estimate_tokens(&self, text: &str, role: &str) -> u32;
}
```

`Message` is the engine's neutral shape: `{ role: String, content: MessageContent::{Text(String) | Blocks(Vec<TextSlot>) | Opaque}, meta: MessageMeta { cache_control: bool, protected: bool, tool_name, file_path, extra: serde_json::Map } }`. `TextSlot` is the key enabling the proxy: a list of independently compressible text spans inside a block-structured message (each Anthropic `tool_result` text block or OpenAI content part is a slot), so `compress_messages` works for block content too — a gateway that can only see whole string bodies compresses none of it.

### 4.7 Stats returned by the engine

```rust
pub struct CompressStats {
    pub tokens_in: u32, pub tokens_out: u32, pub ratio: f32,
    pub chars_in: u64, pub chars_out: u64,
    pub compressed_messages: u32, pub skipped_messages: u32,
    pub by_type: BTreeMap<Kind, u32>, pub mode: Mode,
    pub refs_emitted: u32, pub ref_bytes_emitted: u64,
    pub protected_by_reason: BTreeMap<String, u32>,     // "role:assistant", "frozen", "flagged"
    pub skipped_by_reason: BTreeMap<String, u32>,      // every skip, exactly once (§9.3)
    pub protected_refs: Vec<RefId>, pub protected_ref_reasons: BTreeSet<String>, pub protected_ref_messages: u32,
    pub router_version: &'static str,                    // "algorithmic-v2-rs"
    pub elapsed: Duration,
}
```

`BTreeMap` everywhere output order is observable — never `HashMap` (determinism).

## 5. Engine algorithms in detail

This section is the implementation spec, module by module. Nearly every constant in it was measured into place against a corpus of real agent tool output rather than chosen; where a value was picked rather than measured, the text says so.

### 5.1 Regex translation rules (`engine::patterns`)

Rust `regex` has no look-around and no backreferences, so a pattern that wants either is rewritten:

| Wanted | How it is expressed |
|---|---|
| `\bref:(k_[A-Za-z0-9_-]{12})(?![A-Za-z0-9_-])` | find `ref:k_` with `memchr::memmem`, take the next 12 bytes, verify the class, verify the following byte is not in the class; verify the `\b` before `ref` manually |
| multiline / case-insensitive / dot-matches-newline | the inline `(?m)` / `(?i)` / `(?s)` flags — and a missing `(?m)` silently anchors a pattern to offset 0 |
| `\d`, `\w`, `\s` | Unicode-aware by default in both; keep |
| `str.isdigit()/isalpha()/isspace()` in the token class table | the table covers codepoints < 128 only, so `is_ascii_digit / is_ascii_alphabetic / is_ascii_whitespace` are exact |
| case folding in simhash tokens | `to_lowercase()` — full case folding and lowercasing differ only for ligature-type folds, and the result feeds only the *budget*, never marker bytes |
| the fence pattern `(?ms)^(?P<open>...)\n(?P<body>.*?)(?P<close>^...\s*$)` | same syntax; non-greedy supported |

All patterns are `LazyLock<Regex>`; multi-pattern per-line classification (`LOG`, `BUILD_LINE`, `DIAGNOSTIC`, `SEARCH`) uses `RegexSet` where only membership matters and `Regex` where captures are needed. Where a per-line count over the 1 MB sample is wanted with `(?m)` (log/build/diagnostic counts) we do the same over the sample so counts are identical; both engines treat only LF as the `^`/`$` line terminator under multi-line mode.

### 5.2 Adaptive budget (`engine::adaptive`)

```rust
pub fn adaptive_k(items: &[&str], minimum: usize, maximum: usize, bias: f32) -> usize
```
Tiers as in §7.2. Tokens: `\w+` over `to_lowercase()`. Simhash: per token `Blake2b<U8>` digest → `u64` big-endian; bit votes; Hamming ≤ 3 against the last 256 fingerprints. Bigram coverage with a `HashSet<(u64, u64)>` of token hashes, which counts distinct pairs without materialising them. Compressibility check: `flate2` with the `zlib-rs` backend at level 6, firing only when `len(whole) > 2 * len(prefix)`. The exact compressed length is an implementation detail of the deflate backend and must never reach marker bytes — it only ever moves the budget. Determinism within a build is exact.

### 5.3 JSON handling (`engine::json`)

`serde_json::Value` with `preserve_order` (keys keep input order) and `arbitrary_precision` (numbers keep their source text). Consequences:

- Compact re-serialization: no space after `,` or `:`, non-ASCII left as-is. Numeric literals are **preserved** rather than normalized — a round trip through a parser that turns every number into an `f64` rewrites `1e5` to `100000.0` and `0.1000000000000000005` to `0.1`, which is silent data loss dressed up as reformatting, and invisible to the accounted guard because it cannot tell a rewritten number from re-indentation. `serde_json`'s `arbitrary_precision` keeps the source digits. The guard stays regardless.
- Escaping: both escape `"`, `\` and control characters below 0x20 only; non-ASCII stays raw in both.
- Canonical form for scoring/simhash: keys sorted (BTreeMap conversion).
- Numeric analysis (outliers) parses `Number` → `f64` for scoring only; booleans are their own variant and so are excluded for free.
- `NaN` and `Infinity` are not JSON and `serde_json` rejects them, so such payloads are treated as non-JSON and decline.
- Duplicate keys: the last value is kept at the first position, which is what `IndexMap` does.

### 5.4 Detector (`engine::detect`)

Implements §6 as one function over `sample = first 1_000_000 chars` (char-based slice via `char_indices`, cached on the `Payload`). Structure:

```rust
fn detect_content(p: &Payload) -> Detection {
    if let Some(d) = try_json(p) { return d }                       // parse / NDJSON / truncated-bracket rule
    if DIFF.is_match(s)            { return diff(0.95) }
    if HTML.is_match(s) && looks_like_html(p) { return html(0.95) }
    let diags = DIAGNOSTIC.find_iter(s).count();
    if diags >= 3 { return build(0.9, diagnostics) }
    if diags >= 1 && quoted_source_runs(p.lines()).total_lines() >= 2 { return build(0.85, diagnostics) }
    let search = p.lines().filter(|l| looks_like_search_line(l)).count();
    if search >= 3 && ratio(search) >= 0.6 { return search_results(min(1.0, ratio)) }
    let logs = LOG.find_iter(s).count();        if logs >= 5 && ratio(logs) >= 0.6 { return build(0.85) }
    let builds = BUILD_LINE.find_iter(s).count(); if builds >= 5 && ratio(builds) >= 0.3 { return build(0.8, untimestamped) }
    if TABLE.find_iter(s).count() >= 3 || looks_delimited(p) { return tabular(0.8) }
    if CPP_INCLUDE.is_match(s) { return code(0.7, if CPP_ONLY.is_match(s) { Cpp } else { C }) }
    if CLASS_BRACE.is_match(s) { return code(0.8, if VAR_NS_USING.is_match(s) { CSharp } else { Java }) }
    for (lang, re) in CODE_TABLE { if re.is_match(s) { return code(0.7, lang) } }
    if looks_uniform_stream(p.body_lines()) { return listing(0.8, uniform_stream) }
    if looks_like_listing(p) { return listing(0.75) }
    if let Some(f) = config_flavor(p) { return config(0.8, f) }
    text(0.1)
}
```

`try_json`: stripped sample starts with `[`/`{` → `serde_json::from_str::<Value>(sample)`; on failure, NDJSON (every non-blank line parses, ≥ 2 lines); on failure, if the content is longer than the sample and `content.trim_end()` ends with `]`/`}` → `JsonArray 0.6 sample_truncated`. To avoid parsing a 1 MB sample twice (detector + crusher), `Detection` carries `parsed: Option<Arc<Value>>` when the whole content parsed **[Rust-only optimisation, no behavioral change]**.

Helper predicates are exact ports: `looks_like_html`, `looks_delimited` (first 100 non-blank lines, tab if in the first line else comma, ≥ 3 and ≥ 60% equal counts), `looks_like_listing` (distinct ratio ≥ 0.6; bare entries; offset rows with `offsets_are_addresses`; `looks_aligned` over the first 100 lines with `\S+` field starts, dominant field count ≥ 60%, ≥ 3 offsets shared by ≥ 60%; `looks_aligned_pairs` with `PAIR_MIN_ROWS = 50` and the data-tail test), `looks_uniform_stream` (50 lines, prefix ≥ 12 with ≥ 8 visible chars, tail ≥ 3, ratio ≥ 0.6 over `n-1` pairs, pathless-grep refusal at ≥ 0.6), `config_flavor` (first 200 non-blank lines, ≥ 5), `quoted_source_runs` (gap 3, run ≤ 16, diagnostic on either side), `looks_like_log_line`, `looks_like_search_line` (clock guard on the `path` capture).

### 5.5 SmartCrusher (`engine::crusher`)

```rust
pub fn crush_json(p: &Payload, opts: &EngineOptions, refs: Option<&mut RefCollector>,
                  fallback: Option<&dyn Segmenter>, parsed: Option<Arc<Value>>) -> Option<String>
```
- Parse (reuse `parsed`), else NDJSON, else decline.
- `Lossless` → `crush_lossless(value)`: array factoring → `__cols__/__rows__` with sorted columns; map factoring → `__keys__/__cols__/__rows__` only if shorter; `__literal__` escape for reserved input shapes; no nested routing, no clipping.
- Lossy → `crush(value, depth = 0)`. When `enable_lossy_key_factoring` is on, after lossy recursion each object/array is offered to the same factoring step and the factored form is kept if shorter. The transform is reversible so it needs no omission marker; the accounted guard treats an envelope (`__cols__` present in the output) as a declared reversible transform (`has_envelope`), and `applied` records kind `Factored`. Default **on**; off is the state the recorded vectors in `fixtures/golden.json.gz` were captured under, and the vector test sets it so.
- Arrays: `crush_array` exactly as §7.3 (budget via `adaptive_k` over canonical strings; anchors 30%/15%; scores; must-keeps; one marker at the first omitted index; fragment = compact JSON of the omitted items). Marker text: `... N similar items omitted [ref:k_...] (fields: a, b) ...`.
- Strings longer than `max_field_chars` (chars): `route_nested` (`enable_nested_routing`; depth budget; structured-type set; segmenter first at depth 0; accept only if shorter; rollback on reject); else, for a structured type whose compressor declined, the duplicate fold (`enable_fold_fallback`, same `mostly_repeated_lines` guard as the whole-message router); else keep whole if structured; else `clip` (cut at the last newline at or after limit/2, else the last space at or after limit/2, else at the limit; marker `…[+N chars ref:k_...]`, N counted in chars).
- Output: compact serialization (serde_json's default formatter is already compact).

### 5.6 LogCompressor (`engine::logs`)

Implements §7.4: `crush_log(p, opts, normalize, refs)` with preamble split; lossless forces `normalize = false`; echo arbitration (both folds, smaller wins, loser rollback, winner re-stash); `fold` with the ≥ 50-line scored path (`crush_scored`: ERROR/WARN/SUMMARY regexes with `(?i)`, warning fingerprint, trace-line regex, budget `adaptive_k(lines, 10, min(100, n))`, force-keep of the tail, of marker-carrying lines and of ±3 lines around the first 10 errors, marker `[... N log lines omitted [ref]; X ERROR, Y WARN]`), then template folding with the five normalizers, section separators (including `BEGIN:`/`END:`), blank-line handling, `[×N ref:k_...]` collapse with fragment = occurrences 2..N joined by LF, and the rule "return the input object when nothing folded" (Rust: return `None`).

Templates are computed with the five regexes in order into a scratch `String` reused across lines. The fold index is a `HashMap<&str, usize>` keyed by template text; its iteration order is never observed.

### 5.7 Search, Diff, Tabular, Listing, Config

Implement §7.5–§7.8 and §7.10 including every floor: search ≥ 8 matches; diff ≥ 50 lines and more than one file, the 80% line rule; tabular more than `max(3, max_array_items)` lines, CSV parsed and width-checked; listing 50-entry floor, `budget = max(4, max_array_items)`, head 3/5; config ≥ 3 elidable lines, YAML block-scalar and TOML multi-line bail-outs, INI column-0 only. Diff and listing hold the harness preamble out. CSV parsing: the default dialect (double-quote quoting, no escape char) → `csv::ReaderBuilder` with `has_headers(false)`, `flexible(true)`, `double_quote(true)`; rows must all match the header width or the compressor declines.

### 5.8 Mixed-content segmenter (`engine::mixed`)

Implements §7.11 over **byte offsets** from `split_lines_keepends` (trailing-whitespace trimming walks back by char boundary). Fence regex; JSON spans via `serde_json::Deserializer::from_str(&content[i..]).into_iter::<Value>()`, taking the first item's end via `byte_offset()`, bounded to 256 attempts, span accepted if its char length ≥ `min_nested_chars`. Line kinds, diff-region tracking, aligned-run promotion (`mark_aligned_runs` / `promote` with the named-tail test), the `worth_sampling` placeholder rule. Returns `(String, Vec<Kind>)`; replacements are applied in reverse order into a fresh `String` with `reserve(len)`.

### 5.9 Redraw fold (`engine::redraw`)

`collapse_redraws(&str) -> Option<(String, Vec<&str>)>`: `memchr(b'\r')` fast exit; split on LF only here, not the full separator set — a lone `\x0b` inside a progress frame is payload, not a line break; per line strip trailing CRs as the line ending; skip lines with any frame longer than 256 chars; `resolve_overwrites` by chars (`frame + screen[frame_chars..]` on char boundaries — a frame overwrites *characters*); collect the changed original lines.

### 5.10 Harness preamble (`engine::preamble`)

Up to 10 leading lines matching the key pattern, one terminator that is never allowed to start the frame, and — only *after* that terminator — the truncation notice codex writes under it; returns `(preamble, body)` slices with `preamble + body == content`.

### 5.11 CodeCompressor (`engine::code`)

**Grammars**: all nine languages via tree-sitter, statically linked, Python included. The salvage ladder: whole module → synthetic `if True:` scope (line numbers shift by 1, byte offsets by 9) → longest parseable prefix by line bisection with the tail re-attached verbatim. "Parseable" means no ERROR or MISSING nodes, because tree-sitter never fails outright — it answers a broken file with a tree full of ERROR nodes, so "did this parse" has to be asked as "is the tree clean". That is more tolerant than a strict parser, and the tolerance works in this engine's favour: a uniformly indented mid-file slice reads as a clean module where a strict parser rejects it at line 1, so those payloads are recovered at the first rung rather than the second.

Per language `FUNCTION_NODES` as §7.9; Python nodes `function_definition` and `class_definition` (body field `body`). Body replacement for Python is line-based: `# [... N body lines omitted from <name>; retrieve: ref:k_...]` plus `...` at `col_offset + 4`. Brace languages: byte-based `{ ... }` replacement with a `/* ... */` marker. Skip nested functions; skip nodes with syntax errors; reparse and require `error_count(after) <= error_count(before)`; iterative walk with `TreeCursor`; return `None` when nothing was elided; and return `None` in `Lossless` mode, because eliding a body is not reversible (`strict_lossless`, §4.2).

Grammar loading is a one-time `LazyLock` per language; no warm-up spike beyond first-parse memory. Build: the `code-aware` feature (default on) pulls the grammar crates (roughly +15–25 MB of binary); without it `crush_code` declines everything, which is the intended degradation rather than a build error.

### 5.12 Read lifecycle (`engine::read_lifecycle`)

`classify_reads(&[Message], frozen: usize) -> BTreeMap<usize, ReadState>` (§5.15 step 4): a message whose tool is `read|read_file` and whose path a *later* `edit|write|write_file|apply_patch` targets is `STALE`; paths are normalized before comparison, and reads inside the frozen prefix or marked protected stay `FRESH`. A `SUPERSEDED` state — eliding a read that a *later read of the same file* replaces — is deliberately not implemented: it rewrites content further back in the history and busts prompt caches for a saving the idle-window store already mostly recovers. The proxy populates `tool_name`/`file_path` from tool-call arguments when it can see them (§12.5), which is what makes this reachable at all: a gateway that forwards message bodies without tool metadata can never trigger it.

### 5.13 Token estimator and the guard (`engine::tokens`)

`estimate_tokens(text, role) -> u32` with the §8 weights: a single pass over bytes with an ASCII class table (`[u8; 128]`); non-ASCII counted by `char` and tested against the dense-script ranges (hiragana/katakana U+3040–U+30FF, CJK U+3400–U+4DBF and U+4E00–U+9FFF, hangul U+AC00–U+D7AF, emoji U+1F000–U+1FAFF); role scale 0.775; result `max(1, floor)`. Same constants; `kompressor bench calibrate-tokens` (§18) can refit them from a corpus with `tiktoken-rs` (dev-dependency only).

`TokenGuard`: `Chars` (default) keeps the never-larger check in characters; `Tokens` additionally requires `estimate_tokens(out) < estimate_tokens(in)` and refuses otherwise. Roughly a tenth of compressed payloads shrink in characters without shrinking in tokens, because compression strips cheap letters and spaces while keeping expensive digits and separators, so the remainder is denser than what it replaced. It is opt-in until a corpus A/B moves the default.

### 5.14 The pipeline (`engine::pipeline`)

```rust
pub fn compress_content(&self, p: &Payload, role, forced, refs, ctx, applied) -> (Cow<str>, Option<Kind>) {
    // 0. gutter arbitration: no forced type, enable_line_gutter, len >= min_compress_chars
    //    split the harness preamble; split_line_gutter(body) or fall through
    //    lifted = route(stripped body) then re-attach numbers to surviving lines
    //    plain  = route(original) into a scratch
    //    shorter wins, and the lift must also beat the input; otherwise roll back (§7.12)
    // 1. redraw arbitration: needs refs, no forced type, not lossless, enable_redraw_fold, len >= min_compress_chars
    //    collapsed = collapse_redraws(p); if removed < 128 chars -> route(original)
    //    plain  = route(original)  into scratch A
    //    folded = route(collapsed) into scratch B, plus redraw marker holding the original lines
    //    shorter wins; adopt its scratch; kind = folded_kind.or(detect(original))
    // 2. route(text):
    //    kind = forced or detect (skip -> untouched)
    //    mixed candidate (scratch) if enabled && has_mixed_content -> accounted -> if saving >= 20% adopt & return
    //    whole candidate (scratch) via forced aliases or strategy_for(detection); each compressor call is wrapped in
    //      catch_unwind and a panic counts as "declined" (fail open)
    //    choose: mixed if whole declined or is longer; else whole; else untouched
    // 3. accounted(content, out, kind): lossless exempt; has_marker | has_envelope -> ok;
    //    !whitespace_only -> refuse; saving < 2% -> refuse; else applied.push(Reformat)
}
```

`catch_unwind` around each compressor call is the fail-open promise made concrete: a compressor that panics on an edge case its grammar did not expect counts as having declined, so the payload is forwarded whole rather than taking the request down. A panic increments a metric and logs at WARN with the detection kind, never the payload.

### 5.15 Message-level route logic (`engine::messages`)

`compress_messages(msgs: &mut [Message], req: &CompressRequestOpts) -> CompressStats` lives in the engine crate rather than in any surface, so the API, MCP and proxy share one implementation of conversation-level policy instead of three that drift:

```rust
pub struct RequestOptions<'a> {
    pub frozen_message_count: usize, pub context: &'a str, pub prefix_cache: PrefixCache,
}
```
`frozen_message_count` is read through `effective_frozen()`, which returns 0 on a cold prefix (§5.12b).
Per message: `Opaque` content → skipped; `tokens_in` over all text slots; protection (frozen → flagged → role) → untouched plus a `refs_in` scan into `protected_refs`; stale read (recoverable, collector present, ≤ `max_ref_bytes`) → marker; otherwise each `TextSlot` runs `compress_content` independently (block content) or the single `Text` runs once; finally the cross-message fold (§5.12a) runs on what the message now ships, and the result is recorded for later messages to point at — including for a message that was skipped or protected, which is in context and may be pointed at even though it is never rewritten. Kind, `by_type` and `tokens_out` are accounted here, and so is `CompressStats.elapsed`.

The engine never persists: it returns the collector's pairs and the stats; the caller (server/proxy) persists refs and emits stats. This keeps `kompressor-engine` free of I/O and trivially testable, and is the separation of engine and route.

### 5.12a Cross-message de-duplication (`engine::crossturn`)

Every compressor above reads one payload at a time, so none of them can see the
largest redundancy in an agent conversation: the same bytes shown again in a
*later* message. An agent runs `cat foo.py`, then `sed -n '75,100p' foo.py`,
then `git diff`, then `cat foo.py` again; each block compresses well on its own
while the conversation carries the file three times over.

Measured on 316,526 tool outputs from 3,283 terminal-bench trials, counting only
lines of 40 characters or more: **90 MiB of 549 MiB — 16.4% — is a line some
earlier output in the same trial already showed.** Repeats *inside* one payload,
which the log fold already collapses, are 3.3%. The worst trials run 45–52%.

![cross-message de-duplication](charts/crossturn.drawio.png)

**How it works, end to end.** Take the four turns above. Turn 1 runs `cat foo.py`
and the file — 420 lines — is compressed by whichever compressor claims it, say
the code elider, and *then* recorded: `SeenText` stores the lines the message
actually shipped and indexes every one of 24 characters or more under an FNV-1a
hash of its bytes. Turn 2 runs `sed -n '75,100p' foo.py`. Its 26 lines are
compressed first (a 26-line payload will usually be handed back whole), and the
fold then walks the emitted lines one at a time. For each line long enough to be
an anchor it looks the hash up, gets back the places that line was seen before —
in first-seen order, at most sixteen of them, so a line like `    return` cannot
turn matching into a quadratic scan — and from each candidate walks *forward*
through both the new text and the earlier message in step, counting how far they
agree. The longest run wins, and only its length is used. If
the winning run reaches three lines and 120 characters it is replaced by
`[... 26 lines omitted; shown in full earlier in this conversation]` and the walk
resumes past it; otherwise the line is kept and the walk moves on by one. Turn 2
then records whatever it ended up shipping, markers and all, and turn 3 sees a
corpus built from both.

Three things about that loop are worth stating plainly, because each is a
decision rather than an accident. The fold runs **after** the per-payload
compressor, not before, so the corpus holds shipped bytes — pointing at what a
message *arrived* with would name spans its own compressor then elided. The
fold reads **only earlier** messages, so message *k*'s output does not depend on
message *k+1* existing. And the marker carries **no handle**, because the thing
it names has not been taken out of the conversation at all: it is a few thousand
tokens further up the same prompt, which is the one situation where a retrieval
round trip would be pure ceremony.

`SeenText` accumulates each message's **emitted** text in order — emitted, not
received, because a pointer may only name bytes the model will actually see —
and indexes every line of 24 characters or more by an FNV-1a hash (not
`DefaultHasher`, which is seeded per process; output bytes depend on which lines
match, so the hash has to be as deterministic as everything else here). A later
message's run of ≥ 3 lines and ≥ 120 characters that appears contiguously in an
earlier message is replaced by
`[... N lines omitted; shown in full earlier in this conversation]`. The marker
carries a **count, not a place**: an ordinal would cost a dozen characters on
every turn it survives into, and the model cannot resolve one — it sees a
conversation, not an indexed array. The content is a few thousand tokens above
it, which is the whole address it needs.

Two invariants make it usable:

* **Prefix-monotonic.** Messages are processed in order and matched only against
  strictly earlier ones, so the output of messages `0..k` is byte-identical
  whether or not message `k+1` exists. Appending a turn never rewrites an
  earlier turn, which is exactly what keeps the provider's cache prefix stable —
  the same property that makes every other decision in this engine refuse to
  read the conversation. `the_first_message_keeps_the_bytes_the_second_points_at`
  pins it.
* **The original stays in context.** Only spans present verbatim in an earlier
  message are folded and the earliest showing is never rewritten, so the text a
  marker names is physically above it in the same prompt. That is what makes the
  marker honest *without* a handle: the content was not stashed somewhere, it is
  still on screen. No fragment is minted and no retrieval round trip is needed.

A protected or frozen message is never rewritten but **is** recorded, because it
is in context and is therefore a legitimate thing to point at. An opaque message
pushes an empty entry so ordinals stay aligned with message positions.

`lossless` declines: a pointer is not the bytes, and the mode whose promise is
that nothing is dropped cannot drop a span and name where it used to be, however
findable that place is. A fold that does not shrink the message is refused —
a marker bought for nothing. Gated by `enable_cross_turn_dedup` (default true),
reported as `Kind::CrossTurn` / `cross_turn`.

**What it is worth, measured rather than projected.** Replaying 15 real trials
(511 payloads, 1.73 M chars) through `compress_messages` in turn order: the
per-payload compressors alone remove 39.14%, and the fold takes that to 40.47% —
**+1.33pp**. That is an eighth of the 16.4% line-level ceiling above, and the
shortfall is not a tuning problem, it is forced by the second invariant.

The fold must run *after* the per-payload compressors, because a pointer may
only name bytes an earlier message will actually ship. Deduplicating against
what an earlier message *arrived* with would point at spans that message's own
compressor then elided — a marker naming text that is no longer on screen, which
is exactly the failure the invariant exists to prevent. So the fold sees the
compressed remainder, where most of the repetition has already been taken out by
the fold, the listing sampler and the code elider, and where what survives is
broken into runs shorter than the three-line floor.

Recovering more would mean deduplicating against raw text *and* protecting the
spans a later message points at from the earlier message's own compressor — a
keep-earliest reservation across the whole conversation. That is a materially
larger design and it trades against per-payload ratio; it is not built.

### 5.12b Cache state: the cold prefix (`messages::PrefixCache`)

The frozen prefix exists for one reason: rewriting a byte inside it costs the
cache read for everything after that byte. When the cache has already lapsed
there is nothing left to protect — the provider re-reads the whole prompt at
full price either way — and the bytes the freeze was holding back become the
cheapest tokens in the request to remove.

This is the one place the engine acts on cache *state* rather than assuming the
worst, and it matters because of how the bill is actually shaped: on measured
Harbor runs **94–97% of billed prompt was provider cache reads**, so the token
headline is 12–40× the dollar reality and the cold turn is where real money
moves.

```rust
pub enum PrefixCache { Warm, Cold }          // Warm is Default
impl RequestOptions<'_> { pub fn effective_frozen(&self) -> usize }
```

`effective_frozen()` returns `frozen_message_count` when warm and `0` when cold;
both the protection check and `classify_reads` read it rather than the raw
count. `Warm` is the default and the answer a caller that cannot tell should
use: guessing `Cold` on a live cache costs a re-read of everything after the
first rewritten byte, while guessing `Warm` on a dead one merely leaves a saving
unclaimed — the asymmetry decides the default.

Deciding it is the **caller's** job, because only the caller knows. The prefix
is cold when the reported cache-read token count on the previous response was
zero, or when the gap since the last turn exceeds the provider's cache TTL (five
minutes on Anthropic's default, an hour on the extended one). The proxy is that
caller — `prefixcache::PrefixTracker`, §12.10.

**`cache_control` protects in four places, and lifting fewer than all four
changes nothing.** On Anthropic one marker does every job at once, so the first
three lifts were each individually correct and jointly invisible: measured, the
engine reported `before=10404 after=556` on a cold prefix and the request still
went out byte-identical.

| # | Where | What holds it | How it lifts |
|---|---|---|---|
| 1 | message index | `effective_frozen()` — the marker moved the frozen floor past this message | returns `0` when cold |
| 2 | message flag | `protection_for` → `"flagged"`, from `meta.cache_control` | the `cache_control` arm is `Warm`-only; `meta.protected` is not (a caller said *this message*, not *this cache*) |
| 3 | slot | `TextSlot::protected` on the pinned block | lifts when `TextSlot::protection == WhileCached` |
| 4 | write-back | `SlotOrigin::protected` — `apply_walk` refuses to write a protected origin, so a rewrite the engine *did* make never reaches the body | the proxy clears it for origins whose slot says `WhileCached`, after asking the tracker |

`Protection { Always, WhileCached }` is what carries the *reason* across the
wire-to-engine boundary, because the engine cannot recover it: a cache
breakpoint may be lifted with its cache, while a citation anchor points into
these exact bytes and a thinking signature was computed over them. The walker
assigns it (`anthropic::protection_of`): `citations` or `signature` → `Always`,
everything else pinned → `WhileCached`. A `thinking` block never becomes a slot
at all, so in practice `Always` is reached by citations; the signature arm is
asserted anyway, as the guard for any future block type carrying both.

Layer 4 is cleared in the proxy rather than in the walker because the walker
runs before anything has asked the tracker and its trait takes the body alone;
the reason lives on the slot, so the origin is matched back to it by index.

## 6. Content detection: the ordered chain and every rule in it

`detect_content(content) -> Detection { content_type, confidence, meta }` in `engine::detect`. It looks at `sample = content[:1_000_000]`. **First confident hit wins; order is load-bearing** — nearly every rule exists because a looser rule upstream claimed payloads it then compressed badly. The chain (code):

| # | Type | Rule | Why it is shaped this way (doc) |
|---|---|---|---|
| 1 | `json_array` | Stripped sample starts with `[`/`{` and `json.loads` succeeds (conf 1.0; metadata `item_count`); else every non-blank line parses (NDJSON, conf 0.9, `concatenated_json`); else if `len(content) > 1M` and the content ends with `]`/`}` → conf 0.6 `sample_truncated` (the crusher does the authoritative parse). | Compact documents over the 1 MB window cannot parse from a prefix; they used to fall to text at 0%. |
| 2 | `diff` | `^(diff --git |@@ -\d|--- a/|\+\+\+ b/)` anywhere (conf 0.95). | |
| 3 | `html` | A `<!doctype|html|body|div|table|script` tag **and** structure: a document root (`<!doctype html`/`<html`) or tags on ≥ 20% of non-blank lines (conf 0.95). No compressor — it is a passthrough type. | A Python script was claimed for the comment `# remove <script> tags` (one tag on 9% of lines; real markup 21–100%). |
| 4 | `build` (diagnostics) | ≥ 3 lines matching `^\s*(drive:)?[^\s:]+:\d+(:\d+)?:\s*(fatal error|error|warning|note|undefined reference)\b` (conf 0.9); or 1–2 such lines corroborated by ≥ 2 compiler echo/caret lines within 3 lines on either side and runs ≤ 16 lines (conf 0.85). | Diagnostics are shaped like grep hits and were grouped "by file" with matches dropped. The keyword must *begin* the message so `mod.py:4: raise ValueError("error: bad")` stays a search hit. Echo bounds measured on 164 corpus payloads: 2,039/2,040 echo runs within 3 lines; longest compiler run 14; gcc leads with the diagnostic, rustc trails it. |
| 5 | `search` | ≥ 3 lines and ≥ 60% of lines are grep hits: `^(drive:)?(?P<path>[A-Za-z_./\\][^:\n]*?)[:\-]\d+[:\-].+`, not a log line, and the path does not end in a clock (`[ \t]\d{1,2}$`) (conf = ratio). | Count alone once let an `apt-get` log match at 7.9% "confidence" via hyphenated versions (`cpp-14-x86-64-linux-gnu`); glog `I20260805 07:39:31…` parsed as `path='I20260805 07'`. |
| 6 | `build` (timestamped log) | ≥ 5 and ≥ 60% of lines match `^\s*(\d{4}-\d\d-\d\d[T ]|TRACE|DEBUG|INFO|WARN|ERROR|FATAL|[IWEFD]\d{4,8}[ \t]+\d{1,2}:\d{2}:\d{2}|Mon 5 07:39:31 syslog)` (conf 0.85). The date **must** be followed by a time separator. | `2025-07-03_api.log` is a filename; folding a listing of dated logs as templates destroyed 20 of 25 distinct dates on a date-range task. |
| 7 | `build` (untimestamped) | ≥ 5 and ≥ 30% of lines start (after `[ \t]*`) with a package-manager/build verb: `Setting up `, `Unpacking `, `Get:`, `Processing triggers`, `make[1]:`, `npm WARN`, `Collecting `, `Requirement already satisfied`, `Successfully installed`, … (conf 0.8, `untimestamped`). | A 27.6K apt log compressed 14.7% as text vs 96.6% here; pip's vocabulary and indentation were a gap. Claimed exactly 1 of 143 real payloads — precision lives in the verb list. |
| 8 | `tabular` | ≥ 3 markdown `|…|` rows, or a delimited body (`\t` if the first line has a tab else `,`) where ≥ 3 and ≥ 60% of the first 100 lines share the first line's field count (conf 0.8). | Three matching lines anywhere once made a prose prompt with two commas "tabular". |
| 9 | `source_code` (C/C++) | `#include` anywhere → `cpp` if C++-only markers (`template <`, `using namespace`, `namespace x`, `public:`, `::`, `<iostream|vector|…>`) else `c` (conf 0.7). Decided before the `class` heuristic and the language table. | Of 297 `#include` payloads, the 48 with C++ markers gain 11,470 chars on the C++ grammar; the other 249 lose 3,318 on it. `#include` is exclusive to C/C++. |
| 10 | `source_code` (class) | `^\s*class\s+\w+.*\{` → `csharp` if `var|namespace|using` present else `java` (conf 0.8). | |
| 10a | `tabular` (ratio) | `^\s*\|.+\|\s*$` on **≥ 3 lines and ≥ `TABLE_RATIO` (0.5) of them**, or `looks_delimited`. | The ratio was missing, alone among the rules here — three pipe rows anywhere claimed the payload. Nine corpus payloads at 6–23% (dataset cards, READMEs, curl progress, rustc errors) were handed to a compressor that declines prose. All nine reclassify to `text`; coverage 68.78% → 69.95%, the `tabular` bucket 15 payloads at 28.12% → 6 at 76.96%, and nothing loses ground. `looks_delimited` counts only delimiters **outside quotes and outside any bracket pair**: an argument list has a fixed arity, so `step_11 = compute(entries, 11, offset=154)` repeated down a file matched on 91 of 100 lines and a Python payload was called tabular. Measured on a live codex result, that half of a file went 0.00% → 86.82% once the rule stopped claiming it. |
| 11 | `source_code` (table) | Ordered patterns: python (`^(def |class |import |from .+ import )`), typescript, go, **cpp** (`template <`, `using namespace`, `#include <c?[a-z_]+>`), **c** (`#include|#ifndef`), rust, java, csharp, weak-c (`#ifdef|#define|#endif|#pragma`) (conf 0.7). | `struct ` sent C to Rust, `namespace ` sent C++ to C#, `class ` sent JS to Java; each then parsed under a grammar that found no functions. Weak C tier stays last because `#define/#endif/#pragma` are also C#. A repeated `#endif` is *structure*: left as text, a real header came back `#endif [×37]` with include guards merged. |
| 12 | `listing` (uniform stream) | ≥ 50 non-blank lines, ≥ 60% of consecutive pairs share a ≥ 12-char literal prefix with ≥ 8 visible chars and a ≥ 3-char varying tail; refused when ≥ 60% of lines look like pathless grep (`path.ext:text`) (conf 0.8, `uniform_stream`). | 13,786 lines of `checking cst:compcert.…` compressed 0%: every line unique, none a path. Measured against the *previous* line because these streams walk a namespace. Worth +2.29pp in the current rule set (+3.53pp isolated). |
| 13 | `listing` | ≥ 5 lines, ≥ 60% distinct, and either ≥ 5 & ≥ 60% bare path/name entries (`^/?[\w.@+~-]+(/[\w.@+~-]+)*/?$`), or ≥ 5 & ≥ 60% offset rows (`^\s*[0-9a-f]{3,16}:?\s`) whose offsets look like **addresses** (any `a-f`, or all ≥ 7 digits wide), or aligned columns (≥ 3 shared field offsets across ≥ 60% of rows with a dominant field count), or two-field data rows (≥ 50 rows whose tail is a name/path/quantity) (conf 0.75). | Absolute paths were excluded by the first char class (79-path payload at 0% vs 88%). `nl`/`cat -n`/`grep -n` produce `digits:` over source; sampling those lost 31–89 lines of C at 74–88% "savings" on `fix-ocaml-gc`. Two-field rows kept in their own predicate: lowering `_ALIGNED_MIN_FIELDS` to 2 moved 13 clean listings into `text` (one 611→23,895 chars). |
| 14 | `config` | `config_flavor`: `[section]` + ≥ 2 assignments with ≥ 80% structural lines → `ini` (if any `;` comment) else `toml`; or ≥ 3 `key: value` lines ≥ list items with ≥ 80% structural → `yaml` (conf 0.8). Checked **after** listing. | A `readelf` symbol table satisfies the YAML mapping pattern; its rows are addressable data. |
| 15 | `text` | Everything else (conf 0.1). | |

Deliberately **absent** rules, each rejected with a measurement (doc): a guarded template fold for plain text (+0.56pp, 15 payloads worse, still folded `access-2025-07-03.log`); `grep -n` over a single file (`458:text`) routed to the listing sampler (+0.48pp; the 33 payloads were pattern searches whose ten sampled hits make the call useless).

Shared line predicates `looks_like_log_line`/`looks_like_search_line` are exported for the segmenter so a line cannot be a log to the detector and a grep hit to the segmenter; a log line wins the tie.


### 6.1 Line-numbered payloads

`grep -n` and `cat -n` put a number and a separator in front of every line, and
to the config classifier `27:` is a mapping key — so numbered source reads as
YAML with 0.80 confidence. The config compressor then finds no config and
declines, and because routing has already happened the gutter lift (§7.12) never
gets a turn either. Across seven Harbor runs that was 33 payloads, 213 KB,
returned whole.

When the config rule would fire, `enable_gutter_aware_detection` first asks
`split_line_gutter` whether the payload is numbered, and if it is, classifies the
*stripped body* instead. One level, and it cannot recurse: the splitter requires
strictly increasing numbers and the body it returns has none left to strip. On
those 33 payloads the result is 0% → 53.7% removed, 20 of 33 now compressing; on
the committed corpus ten payloads reclassify and the coverage figure does not
move at all, which is why the flag is pinned off in the recorded vectors rather
than the vectors re-recorded.

## 7. The compressors

Each compressor has the signature `crush_x(content, options, context, refs) -> str` (JSON: `crush_json(content, options, refs, fallback)`), returns the input object when it declines, and stashes elided bytes into the collector only when a collector is supplied (recoverable mode). Shared helpers: `omission_marker(kind, count, fragment, refs, suffix)` → `[... N <noun> omitted [ref:k_…]<suffix>]`, `code_marker`, `redraw_marker`, `adaptive_k`, `split_harness_preamble`.

### 7.1 Harness preamble (`engine::preamble`)

Up to 10 leading lines matching `^(Chunk ID:|Wall time:|Process (exited|running)\b|Original token count:|Warning: truncated output|Total output lines:)`, plus one terminator `^(Output:|New Terminal Output:|Current Terminal Screen:)\s*$` (never allowed to *start* a frame), are split off and re-attached verbatim by the log, code, diff and listing paths. Reasons: `Process exited with code 1` is often the most load-bearing line and scored compressors dropped it; the frame defeats parse-based compression (three C payloads, 24.4K, declined for that alone).

**The frame does not end at the terminator.** When codex truncates a tool result it stamps `Output:` and then writes its notice *underneath*:

```
Chunk ID: ff1912
Wall time: 0.0000 seconds
Process exited with code 0
Original token count: 16692
Output:
Warning: truncated output (original token count: 16692)
Total output lines: 1577

def transform_0(records, options=None):
```

A splitter that stopped at the terminator left those two lines at the head of the body, where they are a syntax error to every grammar — so the code compressor declined and the payload came back whole. Measured on a live codex session: that Python result was **0.00% with the notice in the body and 86.93% without**, and a CSV result reclassified from `text` to `tabular`. Only the two notices are consumed after the terminator, and only there: everything else past it is content. The bound is 10 rather than 8 because the frame is already 7 lines and a limit a real frame sits on is one that fails silently when the next one grows.

The trade is visible in both directions. Holding two more lines out of the body costs the shipped corpus 0.36pp of coverage (16 of its 258 payloads carry the notice, most of them logs that folded those lines away); on a live codex sweep across nine payload types it is worth **+4.67pp overall**, almost all of it one payload going from nothing to 43%. §18.1's `fold_count_unsupported` and the golden vectors both moved with it, each recorded.

### 7.2 Adaptive budget (`engine::adaptive`)

`adaptive_k(items: list[str], minimum, maximum, bias=1.0) -> int`, deterministic and bounded:

1. `n ≤ 8` → `min(n, maximum)`.
2. SimHash dedupe: 64-bit simhash over `\w+` casefolded tokens (per-token 8-byte BLAKE2b, never a hash randomised per process — the output has to be byte-stable across runs), an item is "unique" if Hamming distance > 3 from all of the last 256 fingerprints. `≤ 3` unique → `clamp(len(unique), minimum, maximum)`.
3. Kneedle on the cumulative unique-**token-bigram** coverage curve: for `k ∈ [minimum, maximum]`, maximize `coverage[k-1]/total − k/maximum`.
4. `best = clamp(round(best·max(bias,1)))`.
5. zlib sanity: if `best < maximum` and `len(zlib(all)) > 2·len(zlib(first best))` → `best += 1` (may only increase).

Callers: crusher (min 3, max `max_array_items`), tabular (3, `max_array_items`), search (5, 30), scored logs (10, 100).

### 7.3 SmartCrusher — JSON (`engine::crusher`)

Parse: `json.loads`, else NDJSON as one array (≥ 2 lines), else decline. Output `json.dumps(separators=(',',':'), ensure_ascii=False, default=str)`.

**Lossy modes** (`aggressive`/`recoverable`), depth-first recursion:

- **Arrays** longer than `max_array_items` (10): `budget = adaptive_k(canonical_items, 3, min(max_array_items, n))`. If `n ≤ budget` keep all. Anchors: `first = max(1, round(0.30·budget))`, `last = max(1, round(0.15·budget))` (shrink `last` until they fit). Scores per item: 1.0 if anomalous (error token in any string value/key, or non-2xx `status|status_code|statuscode|code` int), `+0.2` per error token in the canonical text capped 0.6, `0.9` on a shape change vs the previous dict (sorted `(key, type)` tuples) or, for non-dicts, a change in the first 16 `[A-Za-z_]+` words, `1.0` for a numeric field > 2σ from its mean (fields with ≥ 3 samples). Must-keeps (score ≥ 1.0) always kept even over budget ("correctness is preferable to sampling"); the rest fill by `(-score, index)`. One marker at the first omitted position: `... N similar items omitted [ref:k_…] (fields: a, b, c) ...`; fragment = compact JSON of the omitted items.
- **Strings** over `max_field_chars` (256): first offered to **nested routing** (`enable_nested_routing`, `max_nested_depth` 2): `detect_content(text)`; at depth 0 the mixed segmenter is tried first (it only rewrites spans it recognises), then the type's own compressor (`json_array` recurses through the crusher at depth+1; `build`→log; `search`; `diff`; `tabular`; `listing`; `source_code` with language; `config` if enabled, and **arbitrated against the value-normalised fold exactly as the whole-message route arbitrates it** — the config compressor elides values one key at a time, which is the right reading for a hand-written file and close to useless on generated output). Accepted only if shorter; rejected attempts roll back refs. If the type is **structured** (`json_array, config, build, search, diff, tabular, listing, source_code`) and its compressor declined, the string is offered the duplicate fold under the whole-message router's own `mostly_repeated_lines` guard, and kept **whole** only if that declines too — clipping a diff mid-hunk hands the model a corrupt patch, but returning a generated config untouched is how a 72,665-character payload crossed the wire at 0.00% while the same bytes compressed 99.13% as a bare file. HTML is not structured here (explicit clip remains). Otherwise `_clip`: cut at the last newline ≥ limit/2, else last space ≥ limit/2, else at limit; `…[+N chars ref:k_…]`, fragment = the elided tail.
- Scalars pass through.

**Lossless mode** — the only transform is reversible **key factoring**: an array of objects sharing one key set → `{"__cols__":[sorted keys],"__rows__":[[…],…]}`; an object whose values are homogeneous objects (map-of-records) → `{"__keys__":[original keys in order],"__cols__":…,"__rows__":…}`, applied only if shorter. Reserved-shape escaping: an *input* object whose key set is exactly `{__cols__,__rows__}`, `{__keys__,__cols__,__rows__}` or `{__literal__}` is wrapped `{"__literal__": {...}}` so the consumer never mis-reads data as an envelope; the wrap terminates after one level. No nested routing, no clipping, no sampling. Measured 40–46% on map-of-records/ConfigMap bodies (doc). **Open**: factoring is safe in any mode but runs only on the lossless path, so lossy modes get ~0% on map-shaped payloads.

### 7.4 LogCompressor — logs and text (`engine::logs`)

`crush_log(content, options, identity, refs)`, where `identity` is what a line's
repeats are allowed to differ in:

| `Identity` | Folds lines differing in | Given to |
|---|---|---|
| `Exact` | nothing — byte-identical only | plain text, and anything in `lossless` |
| `Values` | timestamps, uuids, hex runs, numbers | every log and every forced-log payload |
| `ValuesAndNames` | also a package, path or artefact name | build output, when `enable_build_templates` |

1. Preamble split.
2. `lossless` → `Identity::Exact`, blanks preserved.
3. **Diagnostic echo arbitration**: if the body carries a diagnostic with ≥ 2 echo/caret lines (`quoted_source_runs`), compute both `_fold(original)` and `_fold(echo-elided)` (each run replaced by `[... N quoted source lines omitted [ref:k_…]]`) and keep the smaller, rolling back the loser's refs. Cannot simply run first: removing 60 echo lines makes the scored sampler keep proportionally more of the remainder (two payloads grew 2,395→4,084). A real gcc payload: 1,873 of 2,334 chars were echo/caret.
4. `_fold`:
   - If `identity != Exact` and ≥ 50 lines: **scored selection** `_crush_scored` when any line has an error/warn/summary word. Scores: ERROR `1.0`, first-seen WARN fingerprint `0.5` (later duplicates `0.05`; fingerprint keeps the text before the first `:`/`=` verbatim and normalizes hex/digits after it), SUMMARY `0.5`, trace lines (`File "…", line N`, `at x(`, `...`, `Caused by:`) `max(score, 0.8)`, else `0.1`. Budget `adaptive_k(lines, 10, min(100, n))`. Always keep: the last `keep_tail_lines` (20), **any line already carrying a marker** (a marker scored 0.1 and was cut, stranding its ref), ±3 lines around the first 10 errors. Fill by score. Append `[... N log lines omitted [ref:k_…]; X ERROR, Y WARN]`. Used only if shorter than the input.
   - The scored reading returns the moment it beats the *input*, except on a
     `ValuesAndNames` payload that carries no diagnostic line at all, where both
     readings are computed and the shorter wins. Scoring returning first is why
     a measured apt transcript compressed 9%: it removed six lines of 389 and
     thereby prevented a name fold that removes 265. Any payload with an error
     or warning in it keeps the scored reading's right of way, because that is
     the reading that preserves them and length alone cannot tell the difference.
   - Else **template folding**: split body/tail by index (`lines[:-0]` bug fixed: `keep_tail_lines=0` used to disable compression). Templates: ISO datetime and `HH:MM:SS[.ms]` → `<ts>`, UUID → `<uuid>`, `(0x)?[0-9a-f]{8,}` → `<hex>`, `\d+` → `<n>`; `Exact` folds byte-identical lines only. Under
`ValuesAndNames` a further pass replaces any token holding both a letter and one
of `. / : _ + ~ -` with `<id>`, so `Setting up libbsd0:amd64 (0.12.1-1build1.1) ...`
and its sixty-six neighbours share one template. **Diagnostics are excluded from
that pass** — `error: foo.c:12` and `error: bar.c:99` differ in exactly the field
it erases — and **a line carrying a marker is never folded at all**, under any
identity: every handle is a token full of separators, so twelve
`[... 2 quoted source omitted [ref:k_…]]` lines shared a template and eleven
fragments ended up reachable only from text the model could no longer see. Blank lines dropped (kept in lossless). A **section separator** (`==== x ====`, `--- x ---`, `==> path <==`, `BEGIN:VEVENT`/`END:…`) resets the fold index — folding across sections moved `Invoice` out of two of three invoices and the task failed ("a count says how often, never in which document"). Repeated lines → `<line> [×N ref:k_…]`, fragment = occurrences 2..N — **but only when the occurrences differ**. When they are byte-identical the suffix is a bare `[×N]`: the count reconstructs them, so a handle beside it offers a round trip for content the marker already carries. Measured on a Harbor run, that rule removes 241 of 275 stored fragments and 88% of the markers the model sees, each holding 64 bytes on average and none ever fetched (§12.6). `fold_handles_identical_repeats = true` restores one fetchable fragment per elision for a caller that wants it. When the occurrences differ and no fragment was stored, a bare `[×N]` would assert a repeat that never happened, and at the default 65,536 it did: 4,000 lines differing only in a number came back as one line and `[×3980]`, against an input holding that line exactly once. Two different situations reach that point and they are answered differently. The store **refusing** a fragment the mode promised to keep is a broken promise, so the group **does not fold**: every occurrence is kept in the place it stood. A mode that **retains nothing by design** (`aggressive`, `lossless`) was never going to have a fragment, so the removal is described rather than counted — `<line> [... N similar lines omitted]`, the omission vocabulary used everywhere else. Refusing there instead would be equally honest and needlessly expensive: measured over the shipped corpus it costs 4.02pp of aggressive coverage against 0.16pp for describing it. `fold_count_unsupported` (§18.1) is the check that keeps it that way. If nothing folded, return the **input object** (rebuilding drops the trailing newline and blanks: 5 of 6 text payloads once "compressed" by 1–8 bytes of whitespace, generating bogus events).

### 7.5 SearchCompressor — grep output (`engine::search`)

Parse each line as `path:line:text` (non-greedy path) or ripgrep context `path-line-text` (greedy path — a single pattern accepting both once split `/app/logs/2025-07-03_api.log:12:` into `path=/app/logs/2025, line=07` and collapsed 41 files into one). Decline if < 8 matches. Group by path; rank files by summed signal (`error|fail|exception|panic|warn|todo|fixme` → 0.51 else 0.01); keep `search_max_files` (8). Budget `adaptive_k(all kept lines, 5, min(30, n))`, per-file allowance `max(2, round(budget·share))`, always keep each file's first and last hit, fill by score. Per-file marker `[... N matches omitted [ref:k_…] in <path>]`; files past the budget get one marker `[... N matches omitted [ref:k_…] across K further files]` — they used to vanish silently (25 files came back as 8). If nothing was dropped, return the input (regrouping is not compression; grep's order is information). Query-relevance scoring against `preceding_user_text` was **removed**: it made 292 payloads (4.85M chars) query-dependent (busting the provider cache every turn) for 3 needles out of 5,786.

### 7.6 DiffCompressor — unified diffs (`engine::diff`)

Preamble-aware. Declines under 50 lines or a single file. Split on `diff --git`; keep the `diff_max_files` (8) files with the most `+`/`-` lines; in kept files keep headers and ±2 lines of context around changes; dropped context in a kept file gets `[... N diff lines omitted [ref:k_…] of context in <path>]` (narrowing a hunk is an omission — 20 context lines once left with no trace); dropped files get one marker. Accept only if line count < 80% of input and shorter. Accepted: the surviving `@@` headers misreport lengths, so the output is not patch-applicable either way.

#### Why it almost never fires, and why that is correct

Three gates compose, and on the diffs agents actually produce all three bite:

| gate | value | what it stops |
|---|---|---|
| `MIN_LINES` | 50 | patches too short to have anything to take |
| `files.len() <= 1` | — | a single-file patch: nothing to choose between, and context alone is not worth it |
| `MAX_KEPT_RATIO` | 0.8 | a shrink under 20%: churn on a cached prefix costs more than it saves |

Measured on four git tasks through the proxy (`harbor_tasks_recoverable_diff`,
2026-09-20, 4/4 resolved). Six payloads carried diff syntax; the detector
classified **all six** as `diff` at 0.95 confidence, and the compressor declined
five:

| payload | lines | files | droppable context | declined by |
|---|---|---|---|---|
| 40,213 c | 63 | 3 | — | `MAX_KEPT_RATIO` — see below |
| 5,441 c | 104 | 2 | 14 (13.5%) | `MAX_KEPT_RATIO` |
| 5,520 c | 98 | 1 | 13 | single file |
| 3,974 c | 83 | 1 | 10 | single file |
| 1,811 c | 17 | — | — | `MIN_LINES` |
| 1,432 c | 16 | — | — | `MIN_LINES` |

**Six of six.** The 40 KB payload does come out 91% smaller, which reads at
first like the compressor earning its place on a large multi-file patch — it is
not. That payload is 63 lines of which **one is 37,328 characters**, a
single-line blob left by the harness truncating its output; the over-long-line
clip takes that line and reports the result as `text`, which is honest, and it
is why `diff` is absent from that run's `by type`. Disable the clip
(`max_line_chars = 0`) and the same payload compresses **0.00%**.

The arithmetic says the gates are right. Across the three that clear
`MIN_LINES` and have real diff lines, context more than two lines from a change
is **10–14 lines of 83–104 — 12 to 13.5%**. Remove every gate and the best case
on a 5 KB patch is ~700 characters before the omission markers are paid for,
against a prefix billing 92% cache reads.

Counting the corpus with those six: of **22 diff-detected payloads, exactly one
has ever been compressed by this compressor** — a 4,803-character patch at 20%,
961 characters, and not a clip artifact. That is the whole measured return.

**Do not treat the empty `diff` bucket as a gap**, and do not read a large
diff-shaped payload compressing well as evidence the compressor worked; check
which transform claimed it. Real agent diffs are one or two files of mostly
changed lines, and `floors.json` has no entry for `diff` deliberately.

### 7.7 TabularCompressor — CSV/TSV/markdown (`engine::tabular`)

Decline if ≤ `max(3, max_array_items)` lines. Delimiter `\t` or `,` from the first line, else markdown (`|` rows, header + separator). CSV parsed; all rows must share the header's width. Keep header, first 2 rows, rows up to `adaptive_k(rows, 3, max_array_items) − 1`, and the last row; one `[... N rows omitted [ref:k_…]]` marker.

The compressor is sound and almost never sees a table: the *detector* claims prose that merely contains one. Measured, cause and cost in [`docs/TABULAR.md`](TABULAR.md) — of nine corpus payloads classified `tabular`, **none is a table**, and routing them by content instead would take 51% off them.

### 7.8 ListingCompressor (`engine::listing`)

Preamble-aware. `lossless` → untouched (dropping entries is the only lever). Listings ≤ **50** entries are sent **whole** (an agent reading back the `gates.txt` it was building got 2 of 31 definitions; all listing compression on the corpus totalled 908 chars, so the floor costs ~0.3%). Otherwise keep `head = 3/5·budget` and `tail = budget − head` entries (`budget = max(4, max_array_items)`), byte-identical, with `[... N entries omitted [ref:k_…]]` between — endpoints reveal the span. Never normalized (that is what destroyed the dates).

### 7.9 CodeCompressor (`engine::code`)

Languages: python, javascript, typescript, go, rust, java, csharp, c, cpp — all nine as statically linked tree-sitter grammars, behind the `code-aware` feature. Without the feature every payload declines and the engine falls open, which is the intended degradation rather than a build error.

Preamble split first, because the harness frame is not part of the file and a grammar reports a syntax error on the whole payload because of it — real C payloads with elidable bodies were returned whole for that reason alone.

**Salvage ladder**, since a payload is usually a *slice* of a file rather than a file: whole module, then under a synthetic `if True:\n` scope for mid-file Python slices, then the longest parseable prefix found by line bisection with the tail re-attached verbatim. 143 payloads / 870K chars in the corpus parse at none of the first two rungs.

**Elision**: function and class bodies longer than `code_max_body_lines` (24) are replaced by a marker and the result is re-parsed.

`code_max_body_lines` is a floor, not a fixed rule. A file whose bodies all sit under it yields nothing at all, and that is not a rare shape: of the eleven corpus payloads this compressor returned whole, **five were Python files whose longest body was 24 lines or fewer**. Lowering the floor for everyone would be the wrong fix — it would elide short bodies in files that already compress 80–96%, trading a marker for a handful of lines. So the collection runs a second time, with a floor of `code_budget_min_body_lines` (6), and **only when the first pass produced no candidate at all**. A file that already compresses is untouched by it, which the corpus confirms exactly: the 21 payloads that compressed before are byte-identical, two of the untouched eleven now compress (+64.14 and +41.75 points), and coverage goes 69.9483% → 70.0766%. Six rather than three: below it the gain is 0.01pp and the cost is a marker on a body that is a signature and a line. (That 70.0766% is what this change measured when it landed; the recorded baseline has since moved to **70.0385%** for the reason in §7.12 — a fold under the gutter lift now mints a handle rather than claiming a repeat the payload does not contain.)

The idea is headroom's `_allocate_body_budget` — allocate against what the file has rather than against a fixed threshold — with one signal deliberately dropped. Its `_analyze_symbol_importance` scores a symbol partly by *how well its name matches the user's prompt*, which cannot be used here: output that depends on the surrounding conversation breaks the prompt cache and the `query_dependent` invariant that guards it (§18.1a). The intrinsic signals it also uses — reference count, fan-out, visibility, naming convention — are query-independent and remain available if this pass is ever made to rank rather than simply take the largest bodies. Python bodies are line-based — `# [... N body lines omitted from <name>; retrieve: ref:k_…]` plus `...` at the body's indent (`pass` reads as a body that deliberately does nothing, so the stub looks like finished code; `...` is what a `.pyi` stub puts where a body is not shown); brace languages are byte-based — `{/* [... N body lines omitted from <name>; retrieve: ref:k_…] */}`. C and C++ names are resolved through declarator chains. Functions nested in functions are skipped, as are functions already containing ERROR nodes.

The walk is **iterative**, not recursive: a 34–40K-line file read under the wrong grammar produces a tree deep enough to exhaust a recursive walker's stack. Acceptance requires the reparse to hold **no more** ERROR nodes than the input, because real C routinely defeats the grammar somewhere and the test has to be relative rather than absolute. Declines when nothing was elided.

### 7.10 ConfigCompressor — YAML/TOML/INI (`engine::configs`)

`lossless` → untouched. Bail on YAML block scalars (`key: |`, `>`, with indicators in either order) and TOML multi-line strings (comments are data there). Elide whole-line `#` comments and blanks (INI: column-0 `#`/`;` only, blanks kept) behind one `[... N comment/blank lines omitted [ref:k_…]]` marker; ≥ 3 lines required. Zero reach on the benchmark corpus (agents read build output, not config files); 22 unit tests are the coverage.

### 7.10-0 Two things the harness frame used to hide

The frame defeats parse-based work, and §7.1 fixed that for the *compressors*. Two places still read the framed bytes:

**Detection.** `try_json` gives up on the first byte — a payload that opens `Chunk ID:` does not open `[` or `{` — so a complete, valid JSON document from any harness that stamps a frame was classified `text`. The JSON test is now run a second time against the frame-free body. Deliberately narrow: `enable_preamble_aware_detection` re-runs the *whole* chain on the body and loses 0.30pp of corpus coverage doing it, because reclassifying is not the same as compressing better. This moves only the one rule that is a parse rather than a ratio, and the measured effect is one corpus payload renamed `text` → `json_array` with its compressed bytes unchanged, because the mixed segmenter was already finding the span. A detector that answers `text` for a JSON array is wrong even when nothing downstream notices.

**`crush_json`.** Same cause, same fix: the frame comes off and goes back on verbatim, as the log, code, diff and listing paths already do it. Forced `--type json` on a framed document went from declining to 74.12%.

### 7.10b The config route is arbitrated against a fold

`crush_config` elides values one key at a time, which is the right reading of a hand-written file and the wrong one of the generated kind an agent actually cats. Measured on a live codex session: 600 repeating `[worker.pool_N]` TOML blocks came back at **0.85%**, and a 700-section INI at **0.00%**, where the value-normalised log fold took **97.96%** and **98.39%** of the same two payloads.

So when the config compressor's own output removed less than `CONFIG_THIN` (70%), the fold is computed too and the shorter ships — the same arbitration the gutter lift and the redraw fold already use, with the loser's fragments rolled back. Above that threshold the config reading stands unchallenged: a compressor that has already taken two thirds of a payload has read it correctly, and folding it as a log would trade that reading for bytes.

The corpus is untouched by this — its 17 config payloads compress at 80.33%, all above the threshold — which is the point: the change reaches generated configuration and leaves hand-written configuration alone. In recoverable mode the fold's occurrences keep a handle, which is what makes it acceptable at all, because a config's values are the whole point of it.

### 7.10c The plain-text fold is arbitrated between two identities

A plain-text fold matched lines byte for byte and nothing else. That is the right reading of prose — normalising numbers would merge sentences differing only in one — and the wrong reading of *generated* text, which is the other thing that arrives classified as plain. Measured on a live codex session: a markdown guide of 80 near-identical fenced blocks came back at **7.15%** exact and **97.10%** normalised, and a 700-section INI its own compressor declined came back at **32.96%** and **98.39%**.

Both readings are now computed and the shorter ships, with the exact one running first and winning ties. That ordering is what keeps prose where it was: a payload whose lines repeat verbatim folds identically under either identity, so the normalised reading can only win where the lines actually differ — which is exactly where it should. The same arbitration is used by the `enable_fold_fallback` path, so a payload its own compressor declined gets both readings too.

Corpus coverage **64.3069% → 68.7817%** (+4.47pp), almost all of it the `text` bucket going from 16.63% to 62.71%. Inspecting those payloads explains the size of the jump: they are R and Python sources and data dumps that the detector calls plain text, where value normalisation is simply the correct reading. `enable_normalised_text_fold = false` restores the exact-only fold. What the normalised reading removes stays behind a handle in recoverable mode, which is what makes it acceptable on anything whose values carry meaning.

### 7.10a HTML (no compressor of its own)

`ContentType::Html` had no arm in `strategy_for`, so it fell to `Passthrough` and every HTML payload was returned whole — detection was precise and nothing acted on it. On a live codex session that was 40,211 characters at 0.00%; across the shipped corpus, 13 payloads untouched.

It now routes to `Strategy::Log` under `enable_html_compressor` (default on). There is no HTML parser and none is claimed: what the log route folds is repeated *lines*, and generated markup is the same element with a number changed, which is exactly that shape. A page written as one long line is left alone like any other single-line payload.

Measured: the live page 0.00% → **94.46%**; corpus coverage 63.1496% → **64.3069%** (+1.16pp), the `html` bucket 2 payloads at 54.67% → 5 at 66.54%, `untouched:html` 13 → 11. The eleven that still decline are pages with nothing repeated, which is the honest answer for them.

In recoverable mode the folded occurrences keep a handle, so the differences between two nearly identical elements stay fetchable. That is the case to watch: a set of XSS test vectors is exactly a run of near-identical markup where the differences are the entire point, and there the fold is only safe because it is reversible.

### 7.11 Mixed-content segmenter (`engine::mixed`)

`has_mixed_content`: a fenced block, or an embedded JSON value ≥ `min_nested_chars` (512) with prose around it, or a ≥ 5-line run of classified lines with prose around it. `crush_mixed` builds byte-offset replacements and applies them in reverse, so every byte outside a span is untouched:

1. **Fences** ```` ```lang … ``` ````: `py|python` → code; `yaml|yml|toml|ini` → config; else re-detect the body and route (`_crush_section`).
2. If no fence shrank: **embedded JSON spans** — a bounded `raw_decode` scan (≤ 256 attempts) accepting every complete value ≥ `min_nested_chars`, wherever it sits (the old rule fired only for a trailing blob with no brace before it).
3. If still nothing: **plain sections** — per-line kinds (`diff` region tracking through hunk bodies and extended headers until a blank line; `build` via the shared log predicate; `search`; `tabular` rows with ≥ 3 delimited fields; `listing` bare entries or hex offset rows; plus promotion of aligned-column runs whose last field names a file), runs ≥ 5 lines. Tabular/listing sections are sampled only if ≥ `max(5, 3·max_array_items)` lines and < 30% of lines carry `<placeholder>`/`{placeholder}` cells (a 15-row CSV template in a task prompt lost 11 rows; the agent guessed the period definitions and failed for a 245-char prize).

Rejected spans roll back their own refs (the whole-message rollback only fires when *no* span shrank). Returns `(content, sorted applied sub-types)`; the route unfolds `mixed` into the real types for analytics (`json`, `log`, not `json_array`/`build`).


### 7.12 Line-number gutter lift (`engine::gutter`)

**A fold under the lift always mints a handle, and that costs something.** Two
lines that are byte-identical once the column is off were not identical in the
payload — the gutter check requires the numbers to increase, so they differed by
at least their number. A bare `[×N]` there claims a repeat the payload does not
contain, so under the lift `fold_handles_identical_repeats` is forced on.
Measured on the shipped corpus: one payload carries 288 numbered lines out of
294 with no long line repeating verbatim, and all 26 of its folds were making
that false claim. Honouring it costs 494 characters of handle on that payload —
`html` 66.54% → 65.69% over 5 payloads, corpus coverage 70.0766% → 70.0385% —
and `fixtures/baseline.json` was re-recorded to match. It is the price of the
marker meaning what it says.

`cat -n`, `nl`, `grep -n`, `sed -n '=;p'` and most agent file-reading tools
prefix every line with its number. That column is four to seven characters wide
and it defeats the **entire** detector chain at once, because every rule in §6 is
a regex anchored at the start of a line:

- `    75\tdef __init__(self, points):` is not a Python definition,
- `696:I20260914 22:08:03 solver.cpp:397]` has no leading timestamp, so it is not
  a log line,
- and a grammar reports a syntax error on the first byte.

So the payload classifies `text`, routes to passthrough, and is returned whole.
Over 1,336 payloads of real agent traffic that happened to 85 of them, carrying
219,451 characters of code and logs the engine compresses well once it can see
them.

**The transform.** Recognise a uniform numeric prefix — at least 5 non-blank
lines, at least 80% of them carrying it (real `grep -n` output interleaves `--`
separators), at most 7 digits, separator `\t`, `:` or `|` but never a space, and
the numbers **non-decreasing**. That last condition is what separates a gutter
from a column of data that happens to be numeric: a `grep -c` tally or an ID
column is unordered, while every tool that numbers lines emits them in order by
construction.

Lift the column, route the body through the normal pipeline, then re-attach the
numbers to the lines that survived by a forward-only walk, so each input line is
claimed at most once. Compressors *select* lines and *insert* markers, so a kept
line matches the input line it came from and gets its number back, while a marker
is emitted bare — which reads correctly, because the gap in the numbering is
exactly what the marker announces. A blank line is never renumbered: it matches
every other blank line, and letting one claim a number would let it claim the
wrong one.

**Arbitrated, not applied**, for the same reason as the redraw fold. Stripping
the column changes what the payload is classified as, and on a payload that was
already routing well that is a change for the worse — `grep -n` output is the
case that proves it, because the search compressor reads `file:line:match`
natively and scores matches against each other. Applying the lift unconditionally
**cost 1.45pp of corpus coverage while raising the claimed share**: more payloads
taken, each handled worse. Computing both readings and shipping the shorter turns
that into +0.61pp on the large corpus and +2.35pp on the small one, across 35
payloads with no regressions.

One fidelity cost is real and worth stating: fragments retained for
`recoverable` retrieval are cut from the *stripped* body, so a fragment fetched
back arrives without its line numbers. The compressed view keeps them; the
recovered text does not.

### 7.13 Over-long line clip (`engine::longline`)

Every compressor in this engine selects, folds or groups **lines**. A payload
whose content sits inside one enormous line therefore defeats all of them at
once, and does it quietly: the detector classifies it, a compressor claims it,
finds nothing it can drop — there is one line, and dropping it would drop
everything — and the payload comes back whole.

On a corpus of 8,617 real agent tool results this was the single largest
remaining loss. 179 payloads that nothing else could touch carried 718,425
characters in lines over 512 characters long: a 21,488-character mount table on
one line, `openssl` progress meters, FASTA and protein sequences, base64 blobs,
and `grep` hits whose matched line is a minified bundle.

**The rule.** A line is clipped when it is longer than `max_line_chars` (512)
**and** contains an unbroken non-whitespace run of at least 120 characters. The
head is kept, the tail is replaced by `…[+N chars ref:k_…]` — the same marker a
clipped JSON string field carries, so a reader does not have to learn two
spellings for "the rest is behind a handle" — and the cut backs up to a space or
newline rather than landing mid-token.

**Why the run, and not length alone.** Length is the wrong test and the corpus
says so. A 1,200-character line of English is long because it is a paragraph;
clipping it throws away readable content, which is the same mistake §7.8's
listing floor exists to prevent. A 1,200-character line containing one
3,000-character token is long because something opaque is embedded in it, and
the tail of that token is almost never what the agent was looking for. Prose has
no 120-character words. Requiring the run gives up 0.77pp of the 2.99pp that
clipping on length alone would take, and in exchange it never cuts a sentence.

**Why it runs last.** It is a fallback, not a stage: it only ever sees payloads
that every real compressor has already declined. That ordering is what makes a
blunt transform safe — it cannot take a payload away from a compressor that
understood it, and it cannot make a good result worse.

It mints a handle per clipped line *before* it can know the finished result is
shorter, so the caller rolls the collector back on the refusal path. Without
that, a refused clip leaves fragments no marker points at: 14 `orphaned_ref` and
`refs_without_marker` violations on the replay corpus, which is how the bug was
found.

## 8. `kompressor-store`: the CCR fragment store

### 8.1 Contract

```rust
pub struct Tenant(pub String);            // "" for anonymous single-tenant deployments
pub struct RefRecord { pub tenant: Tenant, pub id: RefId, pub fragment: String, pub kind: Kind,
                       pub request_id: Option<String>, pub created_at: u64, pub last_accessed: u64, pub ttl_seconds: u64 }

pub trait RefStore: Send + Sync {
    /// Idempotent upsert. Same (tenant, id) ⇒ overwrite fragment, refresh TTL clock, keep `created_at`.
    fn put_many(&self, tenant: &Tenant, refs: &[(RefId, &str)], meta: &PutMeta) -> Result<(), StoreError>;
    /// Point read scoped by tenant. Touches `last_accessed` (idle window restarts) unless `touch == false`.
    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError>;
    /// Batch read for the proxy's protected-ref TTL refresh (§8.4).
    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError>;
    fn purge_expired(&self, now: u64) -> Result<u64, StoreError>;
    fn stats(&self) -> Result<StoreStats, StoreError>;      // live rows, bytes, oldest, per tenant optional
}
pub enum Lookup { Hit(RefRecord), Miss, Expired { expired_at: u64 } }
```

Content-addressed ids, tenant-scoped keys, best-effort writes (a store error is a warning) and fail-open reads (an error is a miss). TTL is an **idle window** restarted on every successful read, capped by an absolute lifetime `ttl × ref_max_lifetime_multiplier` (8× → 8 h at the 3600 s default) measured from `created_at`. `Lookup::Expired` (a row existed but is past its window) is distinguished from `Miss` so `retrieval_events` can record expiries directly instead of joining `protected_ref_events` after the fact.

### 8.2 SQLite backend (default)

File `refs.db` (separate from `stats.db`: different write patterns and retention; can live on tmpfs). Pragmas, applied before the schema: `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, `temp_store=MEMORY`. Column-by-column reference and real rows: Appendix E.1.

```sql
CREATE TABLE IF NOT EXISTS refs (
  tenant        TEXT    NOT NULL,
  ref           TEXT    NOT NULL,           -- 12 url-safe chars, no "k_" prefix
  fragment      BLOB    NOT NULL,           -- UTF-8 bytes (BLOB avoids SQLite text-affinity surprises)
  bytes         INTEGER NOT NULL,
  kind          TEXT    NOT NULL,           -- json|log|search|... (Kind), for stats/debugging
  request_id    TEXT,
  created_at    INTEGER NOT NULL,           -- unix seconds
  last_accessed INTEGER NOT NULL,
  ttl_seconds   INTEGER NOT NULL,
  max_lifetime  INTEGER NOT NULL,           -- seconds from created_at
  PRIMARY KEY (tenant, ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS refs_expiry ON refs (last_accessed);   -- lazy purge sweep
```

- **Concurrency**: `rusqlite::Connection` is `!Sync`, so one connection lives behind a mutex and every operation is serialized within the process. WAL still earns its place — other processes and other connections to the same file (an operator running `sqlite3`, a second instance) read without blocking the writer. There is no separate writer thread and no read pool; the work is short enough that the mutex is not the bottleneck, and the design note that once described one was ahead of the code.
- **Put**: one `INSERT … ON CONFLICT(tenant, ref) DO UPDATE SET fragment, bytes, last_accessed, ttl_seconds, max_lifetime, kind, request_id=coalesce(excluded.request_id, refs.request_id)` per fragment inside a single transaction per batch, followed by the byte-cap check below. `created_at` is deliberately absent from the update list: a re-stash refreshes the idle clock but must not extend the absolute lifetime.
- **Get**: `SELECT fragment, kind, request_id, created_at, last_accessed, ttl_seconds, max_lifetime FROM refs WHERE tenant=? AND ref=?`, liveness computed in Rust so `Expired` can be distinguished from `Miss`. On a live hit with `touch`, an `UPDATE … SET last_accessed=?` runs with the liveness predicate repeated **in SQL** — a touch must not be able to resurrect a row the absolute cap already killed.
- **Purge**: expiry is decided on read (`Lookup::Expired`), and `purge_expired` deletes in bounded batches of 5,000 via `DELETE FROM refs WHERE (tenant, ref) IN (SELECT … LIMIT ?)` — a `WITHOUT ROWID` table has no `rowid` for the usual bounded-delete idiom. **Nothing schedules it**: `refs.purge_interval` (60 s) is currently a knob with no consumer, so dead rows linger until the size cap reclaims them. That is safe but untidy — reads never return an expired fragment, and expired rows do not count toward the cap.
- **Size cap**: `refs.max_bytes` (default 1 GiB) is enforced inline on every `put_many`. Live bytes over the cap ⇒ evict by `last_accessed ASC, tenant ASC, ref ASC` down to 90% of the cap. The 90% is so a store parked exactly at the limit does not evict on every write for the rest of its life; the key tie-break is so that which member of a same-second batch dies does not vary run to run.
- **Recent-write cache** (`store::cache::CachedRefStore`): an in-process write-through cache of the last `refs.recent_cache_entries` (default 4,096) `(tenant, ref) → fragment`, with a 60 s window against an hour-scale TTL. It exists to cover the gap around a *write* — the proxy hands the model a marker and the model may ask for it in the same turn — not to be a second store with its own retention. Reads deliberately do **not** populate it: a record coming back from the inner store does not carry its absolute lifetime, so a cached copy could only guess at the one deadline a read is not allowed to extend.
- Schema versioning via `PRAGMA user_version` and an ordered `MIGRATIONS: &[(u32, &str)]` array, keyed on an **integer**. The usual shortcut — sorting migration filenames as strings — puts `V10_` before `V2_`, so the tenth migration runs first the moment a tenth one exists; keying on an integer and iterating the array cannot express that bug.

### 8.3 Other backends

- `MemoryRefStore`: `DashMap<(Tenant, RefId), RefRecord>` with the same TTL semantics; test default and `--ephemeral` mode.
- `CachedRefStore`: the write-through wrapper described in §8.2, composed in front of either of the above.
- **No Redis backend is built.** `RefsBackend` has exactly two variants, `sqlite` and `memory`. `KOMPRESSOR_REDIS_URL` is still recognised by the environment reader so an operator migrating from a Redis-backed deployment gets a warning ("this build stores fragments in sqlite") rather than silence, but it selects nothing.

### 8.4 TTL refresh for protected markers

`compress_messages` returns the handles found in protected content. The server/proxy issues `touch_many(tenant, ids)` in the background on every request. A marker sitting in a frozen prefix or an assistant echo therefore keeps its fragment alive as long as the conversation keeps being sent, bounded by the 8× lifetime cap. `protected_ref_events` still records the sightings (now with `touched` count) so the effect is observable.

### 8.5 Ranged retrieval

`/v1/retrieve` and the MCP tool accept an optional `range`:

```json
{ "ref": "k_...", "range": { "lines": [120, 180] } }          // 1-based inclusive line window
{ "ref": "k_...", "range": { "json_pointer": "/3/items" } }   // RFC 6901 into a JSON fragment
{ "ref": "k_...", "range": { "bytes": [0, 4096] } }            // char-boundary-snapped byte window
{ "ref": "k_...", "range": { "grep": "ERROR", "context": 2 } } // line filter with context
```

Response carries `content`, `found`, `total_lines`, `total_bytes`, `truncated: bool`. The store always holds the full fragment; slicing happens in the server. `fragment_tokens` in `retrieval_events` records what was actually returned, so the net-savings debit is honest. A `max_retrieve_chars` (default 32,768) truncates and sets `truncated` with a hint to use a range — the anti-loop guard for the negative-net scenario in §9.5.

## 9. `kompressor-stats`: statistics in SQLite

### 9.1 Event model

```rust
pub enum Event {
    Optimization(OptimizationEvent), Retrieval(RetrievalEvent), ProtectedRefs(ProtectedRefEvent),
    ProxyRequest(ProxyRequestEvent), StoreSample(StoreSampleEvent),
}
```

All events carry `time_ms` — **request start**, not insert time, so that a slow write cannot reorder a timeline — plus `tenant`, `request_id`, `surface ∈ {api, mcp, proxy, cli}`.

The HTTP service and MCP stdio process sweep expired fragments at startup and
at `refs.purge_interval`, off the request path, then enqueue a store sample.
Shutdown stops the sweep before draining statistics. The recent-write cache
bounds both its fragment entries and its insertion-order bookkeeping, including
when the same handle is written repeatedly.

### 9.2 Writer

`StatsSink` = `tokio::sync::mpsc::Sender<Event>` (bounded, default 16,384). A dedicated thread owns `stats.db` (`WAL`, `synchronous=NORMAL`) and batches into one transaction every 100 ms or 500 events, using prepared statements. Overflow ⇒ drop + `stats_dropped_total`. Graceful shutdown flushes. `stats.enabled = false` swaps in a `NullSink`. Retention: `stats.retention_days` (default 90) pruning at startup and hourly on the writer thread. Complete UTC days older
than the retention window are rolled up and removed in one transaction;
partial days wait until the next day boundary. Repeated maintenance does not
double-count, including rows rolled up manually before pruning, and late
events add to existing archives. Schema migration 6 records whether each raw
optimization or retrieval row is already included in a rollup. Rollups (§9.5) keep
aggregate history beyond that.

### 9.3 Schema (full DDL in Appendix B; column-by-column reference with real rows in Appendix E.2)

- `optimization_events(id, time_ms, tenant, request_id, surface, api_family, model, provider, mode, tokens_in, tokens_out, chars_in, chars_out, messages_total, messages_compressed, messages_skipped, refs_emitted, ref_bytes_emitted, content_types JSON, protected_by_reason JSON, engine_ms, outcome, skipped_by_reason JSON, frozen_messages)`.

  **`skipped_by_reason` is a partition of `messages_skipped`, not a sample of it** — every skip carries exactly one key, which is what lets it be read as "where the rest went". The keys are a closed set: `opaque` (no text this engine can read), `protected:<reason>` (the `protected_by_reason` key, prefixed so the two maps reconcile without double counting), `below_min_chars` (shorter than `min_compress_chars`; nothing ran, so no type is named), `declined:<type>` (the compressors ran on content detected as `<type>` and none shrank it), and `retrieved_before` (it compressed, and the compression was thrown away because it would have re-elided a fragment the model has already fetched back in this conversation — not a decline, but a deliberate refusal to take tokens that cost more to take than they are worth).

  That last key exists because `content_types` counts only successes. A git workload where every diff is declined by the §7.6 gates reported `diff: 0`, which is indistinguishable from "no diffs were present" — and on one measured run 1,374 of 1,483 messages were skipped with the most interesting fact about the run inside them. The decline path detects a second time rather than widening `compress_content`'s return, which is `Option<Kind>` and `None` for every decline: one extra pass on the declines alone is cheaper than touching every caller, and a declined message is one the compressors have already finished with. `v_skip_reasons` unrolls the map one row per reason.
- `retrieval_events(id, time_ms, tenant, request_id, surface, ref, found, expired, ranged, fragment_bytes, fragment_chars, fragment_tokens, returned_chars, returned_tokens, rolled_up, withheld)`. `withheld` arrived in v10: an ask the per-request token budget refused before the store was read, which is neither a hit nor a miss and is excluded from every rate computed over this table (§12.5b).
- `protected_ref_events(id, time_ms, tenant, request_id, refs JSON, refs_total, reasons JSON, messages, touched)`.
- `proxy_requests(id, time_ms, tenant, request_id, api_family, model, upstream, status, stream, compression_event_id, prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens, shadow_prompt_tokens, retrieve_rounds, retrieve_tokens_injected, upstream_ms, total_ms, error, conversation, rolled_up, strip_usage)`. `compression_event_id` and `shadow_prompt_tokens` are never populated — see E.2 for why the join goes through `request_id` instead. `conversation` arrived in v5, `rolled_up` in v7 and `strip_usage` in v9; this prose listing had drifted three columns behind the ladder. `check_schema` does compare table columns — but against the DDL blocks in E.1, not against prose, so a sentence like this one can rot while the gate stays green.

  **A relayed websocket files one row for the whole connection.** The HTTP path's unit — a request — does not exist on that transport: every frame shares one `request_id`, so the connection is what has an identity. The row carries `status = 101` (the handshake succeeded, and it keeps relayed sessions separable from forwarded 200s), `stream = 1`, the duration, and the error when the relay ends badly. **Token counters are NULL, not zero**, because the relay never reads a `response.completed` frame and so does not know what the provider billed; zero would read as "this cost nothing" and quietly deflate every cost report, the same distinction `opaque` draws in the skip taxonomy. `retrieve_rounds` is genuinely 0, because the retrieve tool cannot be offered on a transport whose tool tree executes inside the client; fragments do come back on that path, but as an append to the client's next frame rather than as a round (§12.5b), so there is no round to count. Without this row a relayed session was invisible to `kompressor stats summary`, the HTML report and the Harbor bench, all of which read `proxy_requests` — it contributed compression events and nothing else, and a relay failure had nowhere to land but a log line nobody tails.

  **`strip_usage` records that the proxy removed the `usage` it had asked for on the client's behalf.** The proxy adds `include_usage` when the client did not, so the retrieve loop can account for rounds the client never sees, then strips the frame back out rather than handing a client a field it did not request — which leaves that request with no usage the client can report. It exists to settle a measured discrepancy: across seven Harbor runs the agent's self-reported prompt tokens matched the proxy's own count to within exactly 13 — the probe request the proxy sees and the agent does not attribute to the trial — on six runs, and on the seventh the agent reported 36,746 fewer (2.3%) with no duplicate row, abandoned request, bad status or retrieve round to account for it.

  **`upstream_ms` is the wait for the provider's response *head*, summed over every send the request made.** Not to the last byte: `reqwest` resolves once the status and headers arrive and streams the body lazily, and for a streamed completion what follows is the model generating while this proxy relays — counting that as provider latency would make every long answer look like a slow upstream. Summed rather than last-write-wins because the retrieve loop sends more than once, and a number describing only the final round would understate the wait by exactly the part the loop added. Accumulated inside `Outbound::send`, the one place every path goes through, which is what stops it drifting from `total_ms`. Recorded on failures too, because a request that was refused after nine seconds waited nine seconds; null only when nothing was sent.

  **`total_ms - upstream_ms` is the proxy's own overhead only on a buffered request.** There the provider's answer is complete when `send` resolves, so the remainder is ours: measured against a mock deliberately sleeping 250 ms, 260.9 ms total against 251.2 ms upstream — 9.7 ms. On a **streamed** request the same subtraction is the provider *generating*, because `send` resolves at the response head and the body follows over the next several seconds. Measured on a Harbor run of 75 requests, 74 of them streamed: the difference works out at a steady **7.5–10.2 ms per output token**, which is a generation rate and not an overhead. Read naively it said the proxy owned 85.6% of the wall clock; the one buffered request in that run put the real figure at 23 ms of 1,223 ms. Separating overhead from generation on a stream needs a per-frame relay timer, which this column is not.
- `store_samples(time_ms, live_refs, live_bytes)` — defined and accepted by the writer, but nothing emits the event yet, so it is empty in practice (E.2).
- `daily_rollups(day, tenant, mode, surface, requests, tokens_in, tokens_out, retrievals, retrieval_hits, retrieved_tokens)` — the engine's own events, archived by `maintain` before it prunes them. `rolled_up` on each event row keeps a day that is archived in more than one pass from being counted twice.
- `daily_requests(day, tenant, requests, prompt_tokens, cache_read_tokens, cache_write_tokens, completion_tokens)` — the billed-prompt side, which `daily_rollups` does not cover. Without it the provider's own accounting vanished at the retention horizon with nothing left behind. What it **cannot** preserve is per-turn ordering, so the incremental-cache identity `read[n] + write[n] == read[n+1]` and the excess-writes figure derived from it do not survive pruning.
- `daily_conversations(day, tenant, conversation)` — which conversations were active on a day, as a **set**. A count cannot be stored directly: a day archived in two passes would sum two DISTINCT counts and double every conversation in both. `v_daily_requests` joins the count back on, so a reader still gets one row per day and tenant.
- Views: `v_daily_savings`, `v_request_economy`, `v_retrieval_hit_rate`, `v_type_coverage`, `v_outcomes`, `v_daily_requests`, `v_skip_reasons`, `v_conversations` — eight (§9.4, §9.5).

Every JSON column is validated `json_valid(...)` by a CHECK constraint. Indexes on `(tenant, time_ms)` for every event table, `(request_id)` on optimization/retrieval/proxy for the join.

### 9.4 Naming the zero (`outcome`)

A request that removed nothing is not a failure, and the number alone cannot say
which kind of nothing it was. Every optimization event therefore carries an
`outcome`:

| `outcome` | Meaning | What to do about it |
|---|---|---|
| `compressed` | something was removed | nothing |
| `prefix_frozen` | every message sat inside the provider-cached prefix | nothing that is worth doing — see below |
| `all_protected` | every message was withheld for a role or an explicit flag | check `engine.protected_roles` |
| `discarded` | the engine shrank it and the serialized body still came out larger, so the original was forwarded | the markers cost more than the elision saved |
| `no_compressible_content` | messages were eligible and no compressor took anything | the one worth chasing |

`discarded` is the proxy's, not the engine's: the optimization row is written
when the engine finishes, and the decision to keep the rewritten body happens
afterwards. On one Harbor run 16 of 92 requests serialized larger than the body
they were given — the markers cost more than the elision saved — and the rows
still claimed the shrink, 2.6% of that run's headline. `Event::Discarded` amends
them; `v_daily_savings` and `v_request_economy` stop counting them, and
`v_outcomes` keeps counting them because how often it happens is worth knowing.

`prefix_frozen` also emits a `warn!`, because it is the case that otherwise
looks exactly like a healthy request. It was found by wrapping a real Claude
Code session: **0 of 178,665 characters removed, six messages, all frozen.**
Claude Code puts a `cache_control` breakpoint on its *final* message — the
correct incremental-caching pattern — so `frozen_message_count` equalled the
message count and no compressor that honoured the cache could touch a byte.

**That floor was a message too coarse.** The provider pins the prefix up to and
including the marked *block*, and Claude Code marks `block[0]`, putting its tool
output after it — outside anyone's cache and withheld for nothing. The walker
now stops the floor before a message whose breakpoint has content after it and
protects that message's blocks up to the breakpoint as slots instead; a message
wholly inside the prefix is reported exactly as before, so where the behaviour
did not change, nothing downstream sees a change. The paragraph below describes
the position both tools started from, and remains an accurate account of it.

That is not an OptScale Kompressor limitation. Headroom, run on the identical prompt
through the identical upstream, reported `requests_compressed: 0` and
`uncompressed_requests: {"prefix_frozen": 2, …}`: the same rule
(`compute_frozen_count` bumps the floor to `i + 1` for every `cache_control`
marker in `messages[i]`, ignoring `system` and `tools`), the same default
(`cache_control_auto_frozen: Enabled`), the same outcome. The taxonomy here is
borrowed from that counter, which was the one thing headroom had and this did
not: it *said* why.

Trading the cache away to compress anyway is a knob both projects expose and
neither defaults to, and the arithmetic says why. Cache reads bill at ≈0.1× (the
`CACHE_READ_RATE` the corpus `billed` metric uses), so removing a fraction `r` of
a prompt that would otherwise have been a cache read breaks even only when
`0.1 = 1 - r`, i.e. at **r = 0.9**. Measured compression on agent traffic is
28–53%. Honouring the breakpoint is not conservatism; it is the cheaper option by
a wide margin.

### 9.5 Economy queries

Gross and net as above, now per **request** because every surface carries `request_id` and the proxy stamps the same id on the compress event, the refs (`refs.request_id`) and the retrieval events it executes:

```sql
CREATE VIEW v_request_economy AS
SELECT o.tenant, o.request_id, o.time_ms, o.mode, o.model,
       (o.tokens_in - o.tokens_out)                       AS saved_gross_est,
       COALESCE(SUM(r.returned_tokens), 0)                AS retrieved_tokens,
       (o.tokens_in - o.tokens_out) - COALESCE(SUM(r.returned_tokens), 0) AS saved_net_est,
       p.prompt_tokens, p.cache_read_tokens, p.retrieve_rounds
FROM optimization_events o
LEFT JOIN retrieval_events r ON r.tenant = o.tenant AND r.request_id = o.request_id AND r.found = 1
LEFT JOIN proxy_requests  p ON p.tenant = o.tenant AND p.request_id = o.request_id
GROUP BY o.id;
```

When the proxy sees provider `usage`, `proxy_requests.prompt_tokens` is the **billed** prompt size and `cache_read_tokens` the cached share; the stats API reports `saved_est` (estimator) next to `billed_prompt_tokens` so the windowed over-counting is visible: every row carries its conversation (§12.10), so a per-conversation unique-bytes view is now *derivable* — `v_conversations` groups by it — though the query that turns it into a headline is not yet written. Until it is, the docs state plainly that windowed gross sums count re-sent history once per turn.

### 9.6 Stats API (served by `kompressor-server`)

Mounted only when `stats.enabled` is true, which it is by default. With
statistics off there is nothing behind these routes, so the whole `/v1/stats`
prefix answers 404 rather than an empty report — an empty report would read as
"nothing happened" when the truth is "nothing was recorded".

| Endpoint | Returns |
|---|---|
| `GET /v1/stats/summary?tenant&from&to` | totals: requests, tokens_in/out, saved_gross, retrieved, saved_net, hit rate, by mode, by surface |
| `GET /v1/stats/histogram?bucket=1h|1d&…` | time buckets with gross/net/retrievals |
| `GET /v1/stats/by_type` | coverage per content type (messages, chars in/out, share of savings) |
| `GET /v1/stats/retrievals?…` | hit/miss/expired counts, ranged share, top refs by retrieval count |
| `GET /v1/stats/requests?request_id=` | one request's economy row plus its events |
| `GET /v1/stats/store` | live refs/bytes, oldest, purge counters |
| `GET /metrics` | Prometheus (§15) |

Responses are JSON; `?format=csv` on the tabular ones. The same queries back `kompressor stats …` on the CLI (direct SQLite, no server needed).

## 10. `kompressor-server`: HTTP compression API

### 10.1 Endpoints (`/v1`, with `/v2` served as an alias so a client pinned to either keeps working)

| Method | Path | Body → Response |
|---|---|---|
| POST | `/v1/compress` | `CompressRequest { messages, model?, provider?, request_id?, options?, frozen_message_count?, context? }` → `{ messages, stats }` |
| POST | `/v1/compress/raw` | `{ content, content_type?, options?, context?, source_tool?, file_path?, request_id? }` → `{ content, stats }` |
| POST | `/v1/retrieve` | `{ ref, range? }` → `{ content, found, expired, total_lines, total_bytes, truncated }` |
| POST | `/v1/detect` | `{ content, role? }` → `{ content_type, confidence, strategy, kind, meta }` (debugging aid) |
| GET | `/v1/stats/*` | §9.5 |
| GET | `/healthz` | `{status:"ok"}` — no dependencies |
| GET | `/readyz` | checks: stats writer alive, ref store open, upstreams configured (proxy) |
| GET | `/metrics` | Prometheus text |
| GET | `/v1/config` | effective non-secret config (auth required) |

`messages[].content` accepts a string, `null`, or an array of blocks. Text blocks are read under all three spellings the wire uses — `text`, and the Responses family's `input_text` / `output_text` — because the proxy's own walker has always read them and this surface did not: a caller posting the shape codex actually sends got HTTP 200 with `tokens_in: 0` and nothing compressed. Measured against a live proxy, the same 43,159-byte payload compressed 15,147 tokens to 1,138 over 26 handles as `text` and compressed nothing at all as `input_text`.  blocks are mapped to `TextSlot`s using the same rules as the proxy's wire walkers (OpenAI `{type:"text"}` parts; Anthropic `text` / `tool_result` blocks) and anything else is `Opaque`, so the API compresses block content too. Extra message fields round-trip untouched, and `frozen_message_count` and message-level `cache_control`/`protected` are honoured.

Error semantics: 400 malformed JSON, 401 missing/unknown key (when auth is enabled), 403 tenant disabled, 413 body over `server.max_body_bytes` (default 64 MiB), 422 schema violation (unknown option, out-of-range), 500 wrapped with a request id, never leaking payloads. Compression errors never surface: the engine fails open and the response carries `stats.errors` counters.

### 10.2 Auth and tenants

`[auth] mode = "none" | "api_keys"`. With `api_keys`, keys come from config (`[[auth.keys]] key = "…", tenant = "team-a", mode = "recoverable", enabled = true`) or a `KOMPRESSOR_API_KEYS` env (`key:tenant,...`), matched from `Authorization: Bearer <key>`, `X-Api-Key`, or `X-Kompressor-Key`. Reading only the bearer form would leave a client that authenticates the way the Anthropic SDK does — `x-api-key` and nothing else — presenting no credential this service recognises, and unrecognised traffic is silently never compressed rather than loudly rejected. The tenant is the scoping unit for refs and stats; keys are never persisted in stats (only `tenant`). `mode = "none"` means a single anonymous tenant (`""`), the default for local use. A `X-Kompressor-Tenant` header is honoured only from trusted networks (`auth.trusted_proxies`) or with the cluster secret header `X-Kompressor-Secret`.

### 10.3 Request handling

Handlers: parse → merge options (tenant override → request `options`) → `spawn_blocking(engine.compress_messages)` → respond → background: `refstore.put_many`, `refstore.touch_many(protected_refs)`, `stats.send(Optimization)`, `stats.send(ProtectedRefs)`. Payload bodies are logged only when `log.payloads = true` (DEBUG), never by default. Concurrency bounded by `server.max_inflight_compressions` (default = CPU count × 2) via a semaphore; excess requests queue up to `server.queue_timeout` (2 s) then 503 with `Retry-After` — the API caller (unlike the proxy, §12.8) is expected to pass the original through on 503.

## 11. `kompressor-mcp`: MCP server

Built on `rmcp` (server + `streamable-http-server` + `transport-io`).

### 11.1 Tools

| Tool | Input | Output |
|---|---|---|
| `optscale_kompressor_retrieve` | `{ ref: string, range?: Range }` | `{ content, found, expired, total_lines, total_bytes, truncated }` |
| `optscale_kompressor_compress` | `{ messages: Message[], model?, provider?, mode? = "recoverable", options? }` | `{ messages, stats }` |
| `optscale_kompressor_compress_raw` | `{ content, content_type?, mode? = "recoverable", options? }` | `{ content, stats }` |
| `optscale_kompressor_stats` | `{ from?, to? }` | the `/v1/stats/summary` payload for the caller's tenant |

Tool descriptions and the server `instructions` spell out every marker shape from Appendix B and the rule "on a `ref:k_…` marker call `optscale_kompressor_retrieve(ref)`; prefer a `range` when you need one record or a line window". The `retrieve` description also says a miss means the data expired and must be regenerated (re-run the command), never invented.

### 11.2 Transports and auth

- **Streamable HTTP** at `/mcp` on the shared listener: stateless — each POST independent, JSON responses — so the surface scales horizontally without sticky sessions. The header set read for tenant resolution is the same as §10.2. Origin/Host checks are configurable (`mcp.allowed_hosts`, default any, because the usual deployment is an internal service reached by its own DNS name). An SDK default of localhost-only host checking rejects exactly that request as misdirected: the caller's connection hangs and the retrieval capability silently never registers.
- **stdio** via `kompressor mcp serve [--config PATH]`. An agent runtime spawns this process and speaks JSON-RPC down the pipe, so there is no connection to read a credential from: the tenant is fixed for the life of the process and resolved once at startup, through the same `Authenticator` the HTTP surface uses (`KOMPRESSOR_API_KEY`, or `KOMPRESSOR_SECRET` with `KOMPRESSOR_TENANT`). It opens the same SQLite files a running `serve` uses, WAL making one writer and many readers safe across processes, so a local proxy and a stdio tool server share one set of retained fragments and a handle minted by either resolves through the other. Logging is deliberately not initialised on this path: tracing's default sink is stdout, and one stray line there is a protocol violation the runtime reports as a broken server.

### 11.3 Session hints

rmcp's stateless mode has no per-session memory; retrieval loops are bounded by the caller. We add `optscale_kompressor_retrieve` rate limiting per tenant (`mcp.max_retrievals_per_minute`, default 120) as the only server-side guard against runaway loops on this surface.

## 12. `kompressor-proxy`: LLM proxy mode

The proxy is what makes `recoverable` safe as a default: it compresses on the way in, gives the model a way back, and executes that way back itself. Collapsing the compressing hop and the retrieval hop into one in-process path is the structural point — a gateway plugin that compresses, plus a separately registered tool server that retrieves, is two components that can disagree about the mode, the tenant, the token estimator and whether the retrieve loop ran at all.

### 12.1 Routing

```toml
[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"                                 # chat/completions and responses are compressed;
base_url = "https://api.openai.com"                 # models, embeddings and the rest stream through
api_family = "openai"                               # advisory only — see below
forward_auth = true                                 # pass the client's Authorization/x-api-key through
# headers = { "OpenAI-Organization" = "org_..." }   # optional forced headers

[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/v1/messages"                        # before the openai mount: /v1 also matches this
base_url = "https://api.anthropic.com"
api_family = "anthropic"

[[proxy.upstreams]]                                 # named prefix routing for multi-provider setups
name = "local-ollama"
path_prefix = "/ollama/"
strip_prefix = true
base_url = "http://127.0.0.1:11434"
api_family = "openai"
```

`path_prefix` and `strip_prefix` are flat keys on the upstream, not a nested
`match` table, and the configuration refuses unknown keys — so a stray `match =
{ … }` is a parse error rather than a silently ignored mount.

One mount may route two ways. Routing is otherwise by path alone, which is the
rule a client can reason about from the outside; subscription traffic breaks
that rule at the provider rather than here. Codex sends the same Responses body
to `api.openai.com/v1/responses` under an API key and to
`chatgpt.com/backend-api/codex/responses` under a ChatGPT plan, and only the
credential distinguishes them — so an upstream may carry an override that the
proxy selects when `authmode::classify` reads the caller as a subscription:

```toml
[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "https://api.openai.com"
api_family = "openai"

[proxy.upstreams.subscription]                      # optional; path-only routing without it
base_url = "https://chatgpt.com"
rewrite_prefix = "/backend-api/codex"               # replaces path_prefix upstream; empty moves only the host
```

The rewrite replaces the mount prefix rather than stripping it, and the result
still has to be a path `detect_family` recognises — otherwise the body is
forwarded whole and every health check stays green. That is why
`…/codex/responses` and `/backend-api/responses` are listed there explicitly.
Getting this wrong is not a lost optimisation: a subscriber forwarded to
`api.openai.com` receives a 404, and Codex reads a 404 on that surface as an
invalid session and stops using the subscription at all.

`api_family` defaults to `"auto"`, which detects the wire family from the path
the upstream will see. `"passthrough"` explicitly disables body parsing; other
accepted values document the provider without overriding path detection.

Longest matching upstream path prefix wins; unmatched paths → 404 (or `proxy.default_upstream`). Request/response bodies for non-compressible endpoints are streamed through untouched.

### 12.2 API families and where the text lives (`kompressor-wire`)

Each family has a **walker** that maps a parsed body (`serde_json::Value`, preserve_order) to the engine's `Message`/`TextSlot` view and writes changed slots back **in place**, leaving every other byte of the JSON tree as parsed (the body is re-serialized once; with `preserve_order` + `arbitrary_precision` this is a faithful round-trip of key order and numbers; whitespace normalization of the *request* JSON does not affect provider caching because providers cache on tokens, not bytes).

| Family | Endpoint | Compressible slots | Protected by construction |
|---|---|---|---|
| OpenAI Chat | `/v1/chat/completions` | `messages[i].content` string; `content[]` parts `{type:"text"}`; role `tool` messages (`content` string/parts); role `user` | `system`/`developer`/`assistant`; parts with any other type (`image_url`, `input_audio`, `file`) |
| OpenAI Responses | `/v1/responses` | `input` string (as one user message); `input[]` items `{type:"message", role:"user", content:[{type:"input_text"}]}`; `{type:"function_call_output", output: string | [{type:"input_text"}]}`; `{type:"custom_tool_call_output"}` | `instructions`; assistant messages; `reasoning`, `function_call`, `item_reference`, images/files; `previous_response_id` requests are **not compressed** (server-side history we cannot see) |
| Anthropic Messages | `/v1/messages` | `messages[i]` role `user`: content string; blocks `{type:"text"}`; `{type:"tool_result", content: string | [{type:"text"}]}` | `system` (string or blocks), assistant turns, `thinking`/`redacted_thinking`, `image`, `document`; any block with `cache_control`, `citations`, `signature`; **all messages at index < frozen floor** |

**Frozen floor** (Anthropic): `frozen_message_count = max(i)+1` over messages containing a block with `cache_control`; markers in `system`/`tools` do not move the floor. OpenAI has no explicit markers, so the floor is **0** and the engine may touch the whole history. That is not a setting: `proxy.openai_frozen_prefix` is described in §12.2–§12.3 and listed in §19 as absent from the configuration, and it is — there is no such field, so `none` is the only behaviour available. It is also the one worth having, and it is now measured rather than argued: on a 53-turn gpt-5.5 run the engine rewrote all history every turn, removed 49.4%, and the provider still served 95.4% of the billed prompt from cache (median 97.2% over turns 4–53, minimum 85.6%). Byte-stable output and prompt caching are not in tension.

Tool-call **arguments** in assistant turns are never touched; tool **outputs** are the prime target. The walker records `tool_name`/`file_path` for a tool output by looking up the matching `function_call`/`tool_use` by `call_id`/`tool_use_id` in the same request and reading `name` and `arguments.{path,file_path}` — which feeds the read-lifecycle classifier (§5.12).

### 12.3 Request pipeline (per proxied completion)

```
1. accept; assign request_id (X-Request-Id passthrough or ULID); resolve tenant (§10.2) and mode
   (X-Kompressor-Mode header → tenant → config); mode off ⇒ jump to 6 with the original body
2. read body up to proxy.max_body_bytes (64 MiB default); parse JSON; unknown shape ⇒ passthrough
3. walk (family) → Messages/TextSlots + frozen floor + tool metadata
4. quick gate: Σ estimate_tokens(compressible slots) < proxy.min_tokens (512) ⇒ passthrough
   (the same estimator the engine uses, so the skip decision and the reported saving answer the same question)
5. spawn_blocking(engine.compress_messages) with a hard deadline proxy.compress_timeout (default 2 s;
   an out-of-process hop would need far more) ⇒ on timeout/panic the original body is used (fail open)
   write slots back; if any marker present and inject_retrieve_tool ⇒ add the tool definition (§12.6)
   stash refs into the write-through cache + async store; enqueue stats
6. forward upstream (reqwest, streaming), header policy (§12.7); if stream ⇒ SSE relay (§12.6/§12.9)
7. capture usage (usage object or final SSE chunk); enqueue ProxyRequest event; touch protected refs
```

The compressed request is what the provider bills, so `proxy_requests.prompt_tokens` is the true cost of the compressed prompt. To give operators a **measured** rather than estimated saving, `proxy.shadow_sample_rate` (default 0) optionally sends `1/N` requests uncompressed as well (to a `/v1/…` token-count endpoint where the provider offers one — Anthropic `count_tokens`, otherwise skipped) and records `shadow_prompt_tokens`. That is the only way to report a saving that is measured rather than estimated — everything else compares the compressed prompt against a token count nobody was billed for — and it is off by default because it costs money.

### 12.4 Never-larger and cache stability at the proxy layer

- A compressed body that is not smaller in estimated tokens than the original is discarded (the engine's per-message guard already ensures this per slot; the proxy re-checks the total and also checks byte length).
- Determinism is inherited from the engine; the walker must not introduce nondeterminism (no `HashMap` traversal into output; `preserve_order`).
- Injected tool definitions are appended in a fixed position (end of `tools`) with fixed bytes so they do not perturb the provider's cached prefix across turns; for Anthropic, the injected tool is placed **after** any tool carrying `cache_control` and never receives one itself.

### 12.5 Tool metadata for the read lifecycle

When a tool output's originating call names a file-reading tool (`read_file`, `Read`, `cat` via a shell tool is *not* inferred) and a later call in the same request names a writing tool on the same normalized path, the earlier output is `STALE` and, in recoverable mode, replaced by the stale marker with a ref. `proxy.stale_read = true` by default; `superseded` not implemented (§5.12).

### 12.5a Wiring a subscription client

![subscription wiring](charts/subscription.drawio.png)

The credential stays the client's throughout. OptScale Kompressor never mints, stores or inspects one — `forward_auth` relays the header it was given, and the only decision the proxy makes about it is which host it belongs to.

**Pointing the client at the proxy takes a different lever per client.** Claude Code reads `ANTHROPIC_BASE_URL`, set to the root with no `/v1` because it appends its own. Codex reads no such variable: `OPENAI_BASE_URL` is ignored, measured on 0.155.1 where a wrapped session sent every request straight to the provider, so its config file has to be edited instead. Under a ChatGPT plan the one key that works is `openai_base_url` at the very top of `~/.codex/config.toml`, because codex routes plan traffic through its built-in `openai` provider whatever `model_provider` says, and only the top-level override catches it.

**The trap is on the codex side and it is a hard failure, not a degradation.** Do *not* add `model_provider` or a `[model_providers.*]` block under a plan: codex honours it, selects that provider, finds the `env_key` it declares and refuses the session with `Missing environment variable: OPENAI_API_KEY` before sending anything. An API-key session wants exactly the opposite — the custom provider block, which is the only place `supports_websockets` can be set. `kompressor wrap codex` reads `auth_mode` from `~/.codex/auth.json` and writes whichever block fits; a missing or unreadable `auth.json` falls back to the API-key block, which is how a logged-out install produces that error message.

**The proxy then classifies the caller from two signals, and needs both.** `authmode::classify` calls a request `Subscription` only when a known CLI User-Agent arrives *without* a per-token key. The User-Agent alone is wrong because the same binaries run against plain API keys; the token shape alone is wrong in the other direction because Claude Code carries `sk-ant-oat…`, which begins like an API key and is not one — so that prefix is tested before `sk-`, a three-segment JWT counts as OAuth, and a non-Bearer scheme is a signed cloud credential. This matters for safety and not only for accuracy: the classification chooses the *host*, and requiring the absence of a per-token key is what stops a codex session holding an OpenAI API key from being routed to `chatgpt.com`.

**The mount then follows the credential.** A ChatGPT plan puts the same Responses body on a different host under a different prefix, so the `openai` mount carries a `[proxy.upstreams.subscription]` override and the proxy selects it for subscription callers: `POST /v1/responses` reaches `api.openai.com/v1/responses` on a key and `chatgpt.com/backend-api/codex/responses` on a plan. The mount prefix is *replaced* rather than stripped, because what follows it is the endpoint the client asked for and the provider keeps that part intact under both credentials. Omitting the override does not merely lose compression: the plan session is forwarded to `api.openai.com`, which 404s, and codex reads a 404 on that surface as an invalid session and stops using the subscription at all.

**The Anthropic mount carries no such override, and that is not an omission.** A Max or Pro plan and an API key both reach `api.anthropic.com`; only the header policy differs. The asymmetry belongs to the providers, not to this design.

**What a subscription caller gets on the wire.** It is relayed as close to byte-identical as the proxy can manage, because the provider recognises its own client. OptScale Kompressor adds nothing of its own — not even `X-Kompressor-Request-Id`, which still reaches every stats row and log line but never the wire (§12.7).

**What it is worth, and what it is not.** A plan bills no tokens, so compression saves no money on this path. What it buys is context headroom: on a live ChatGPT-plan codex sweep the final turn carried **37,501 tokens instead of 235,036**, 6.3x more conversation before the window fills. There is also one wrinkle — `openai_base_url` overrides the built-in provider's URL but cannot disable its WebSocket preference, and no top-level key can, so a plan session always opens a websocket. An API-key session, which goes through the provider block, does not. What happens next is §12.5b's flag: on, which is the default, the upgrade is relayed and the session keeps the socket it opened; off, it reaches the forward path as a plain GET, the provider refuses it on a path that takes POSTs, and the session pays a five-attempt handshake (405 each) before falling back to HTTPS. That 405 is the upstream's, not ours. Whether subscription traffic should be rewritten before it is sent is the operator's decision, not this document's.

### 12.5b The websocket relay

`proxy.websocket_relay` (default `true`) relays an upgrade on a mounted path to the upstream instead of letting it fall through. It compresses the one frame shape it understands — `response.create`, which carries a whole request in a single JSON message — and relays everything else byte for byte.

**It gates one branch and nothing else.** The condition is `websocket::is_upgrade`, which requires both `Connection: upgrade` and `Upgrade: websocket`; an ordinary request carries neither, falls past it, and is routed, compressed, measured and offered the retrieve tool exactly as before. One listener serves both transports at once — a subscription codex on a socket and an API-key client over HTTP — and enabling either has never disabled the other.

**The default used to be `false`, and the measurement behind that is now history.** One sweep, same fixtures, same stages, only the flag different — taken *before* the relay compressed anything, which is the state these numbers describe:

| | 405s | fallbacks | relays | compression events | tokens saved |
|---|---|---|---|---|---|
| off | 21 | 1 | 0 | 23 | **1,768,634** |
| on | 0 | 0 | 1 | 0 | **0** |

Those zeroes are a property of the *first* relay, which only forwarded. This one puts `response.create` through the same engine the HTTP path uses, so that row no longer holds — and it has not been re-measured since, which is the number this section is missing. Read the table as the shape of the risk rather than as a current result: everything saved on this transport depends on the relay recognising the frame, and whatever it does not recognise crosses the wire whole.

**What relaying still costs.** Two things, both structural rather than unfinished work.

*No provider usage.* The relay never reads a `response.completed`, so `prompt_tokens`, `cache_read_tokens` and the rest are filed **null, not zero** — zero would read as "this cost nothing" and deflate every cost report, while null reads as "not measured from here", which is the truth. Traffic on this transport is therefore compressed and unmeasured: a relayed sweep is read on frame counts, and the Harbor token table has nothing to say about it.

*No retrieve loop*, for the reasons below.

**Why it is on.** Off, every token saved on the codex path depends on the client choosing to retry over HTTPS after its own transport fails. That retry is not ours and never was: a build that drops it, or ships with it disabled, reaches the provider over a socket an off relay does not touch — nothing compressed, every health check green, no error anywhere, which is the silent failure §14.1 is organised against arriving through the transport layer rather than through a variable name. On, that case is compressed, and what it costs is the token counts. An operator who would rather have the numbers than the coverage turns it back off with one variable: `KOMPRESSOR__PROXY__WEBSOCKET_RELAY=false`.

**Retrieval without a tool call.** Over HTTP the proxy offers `optscale_kompressor_retrieve`, holds the whole response, answers the call and runs a second round invisibly (§12.6). None of that ports, and a capture of a live codex session (481 frames, 10 requests) says why rather than leaving it to judgement. Tools are declared **once**, in `input[0]` of the first `response.create`, as an `additional_tools` tree — `functions`, `clock`, `collaboration`, `mcp__cua_repl` — whose only executable member is `exec`, a JavaScript isolate that runs *inside the client*. A tool this proxy invented would be dispatched there, against a handler that does not exist, and the call would never reach the wire: in that capture all eight tool calls were `exec`, and the nested `exec_command` each one ran never appeared as a frame at all. Nor is there a round to run. The chain is stitched with `previous_response_id` under `store: false`, so a response the proxy created on its own is one the client never names again, and the client's next frame would orphan it — unless the relay rewrote every subsequent `previous_response_id`, which is a second implementation of CCR rather than a port of the first.

What the relay does have is every `response.create`, and those accept more than one input item — the second frame of that capture carried five. So retrieval here is a **piggyback** (`piggyback.rs`). The relay remembers the handles it elided on this connection, watches the model's own output deltas for one coming back, and appends the original to the client's *next* frame as a developer message. No tool, no extra round, no rewritten ids. The handle arrives split across deltas as often as not, so the scan runs over a rolling window rather than per frame. A second signal covers the model that describes what it wants without quoting the handle: the same two-words-in-common test §12.6 uses to keep a fragment inline, read from the **newest input item** rather than the frame. Both narrowings are measured, not stylistic. The frame is no use — the first of a session is 67 KB of which 46 KB is the tool tree — and neither is the whole of `input`: the second frame of that capture carries 10,754 characters of text, nearly all standing developer instructions, against 196 for the turn alone, and against that much prose almost any fragment shares two words and everything elided looks wanted. Each fragment is answered once — a model repeating a handle is not asking twice, and paying for it on every remaining frame is precisely the loop §12.6 was fixed for — and whatever has been handed back is passed to the compressor as `keep_inline`, so the next frame cannot elide it straight back out.

**The order of the three passes is load-bearing, and the middle one was found by a test rather than by reasoning.** What the turn is about is read from the client's own text *before* the engine rewrites it, and weighed only against handles from earlier frames. Read afterwards it would be weighed against handles minted on the same frame, and a compressed log still carries the samples the crusher kept — which share every word with the fragment behind them, so every fresh handle looks wanted and the relay restores what it has just elided, on the frame that elided it. That frame then goes out *larger* than it came in: compression undone in the name of retrieval. The append, conversely, comes after the engine, because a fragment put in before it is content the engine weighs up and, on a frame with enough else in it, elides straight back out under a fresh handle — the span minted around the wrapper text is not the one `keep_inline` protects. Appending last cannot be undone and costs nothing, since the client does not know about the item and never sends it back.

**Which `proxy.*` settings reach this transport.** A knob that silently does
nothing on one path is the §14.1 failure wearing configuration as a hat, so the
answer is written down rather than left to be discovered:

| setting | relay | |
|---|---|---|
| `enabled`, `min_tokens`, `compress_timeout`, `stale_read` | **yes** | the same compressor |
| `retrieve_tool` | **yes** | `never` turns the piggyback off too |
| `retrieve_max_tokens_per_request` | **yes** | per *connection*, which is what this transport has instead of a request — the same reason it files one `proxy_requests` row |
| `retrieve_max_rounds`, `retrieve_execution` | n/a | there is no round and nothing to execute server-side |
| `force_usage` | n/a | the relay never reads a `response.completed`, so there is no usage to force |
| `stream_*` | n/a | the SSE state machine, which this path does not use |
| `frozen_prefix` | **ignored** | deliberate: the relay tracks no prefix, so it compresses every frame `Warm` |
| `max_body_bytes`, `upstream_connect_timeout` | **not applied** | a known gap, not a decision — frame size and dial time are bounded by the websocket library's own defaults rather than by configuration |

The budget is the one that matters in practice. Measured on the sweep that
prompted it, of three relayed connections one handed back **60,086 tokens**
against the default 16,384 while the other two came in at 12,705 and 12,985 — so
the cap binds on the case that needs it and leaves the ordinary one alone. A
fragment withheld is logged at `warn`, because a withheld fragment and one the
model never asked for look identical from outside, and an operator tuning the
number has to be able to see it bind.

A log line was not enough. Every retrieval figure is computed from
`retrieval_events`, and a refusal filed no row — the gate is checked before the
store is read, deliberately, so that content the model never receives is never
counted as returned. The consequence was that the budget was the one part of
the retrieve loop with no trace in the database: a relayed connection that
restored 7 fragments and was refused 11 reported `hits 7 (100.0%), 0 missed`,
because the rate was computed entirely over the asks that got past the gate.
Schema v10 adds `retrieval_events.withheld`, filed by all three refusal sites —
the buffered path, the SSE path and the relay, the first two through
`Retriever::refuse`.

It is a flag of its own rather than another miss. `found = 0` means the store
did not have it, which is an expiry or a handle the client invented; folding a
refusal in would make a tightened budget read as a store regression. Every
existing rate therefore excludes these rows and the count is reported beside
the rate, never inside it — so the hit rate keeps meaning "of the asks the
store was given, how many it answered", which is what it always meant. A
withheld row carries no sizes, because nothing was read.

What the counting then showed is in §20, item 10: the default refuses 10 of 17
asks on this workload, and lifting it to 65,536 leaves 71–79% of the gross
saving where the default leaves 92–95%. 65,536 turns out to be the off switch
rather than a looser ceiling — demand caps at exactly 60,086 tokens, the same
figure an uncapped relay produced before the budget existed. Which value is
right is not settled, and settling it needs a sweep that scores answers.

The asymmetry is worth naming rather than discovering. The fragment arrives as *new input*, not as a repair to the turn it was taken from: the elided marker stays elided in the server's stored prefix, which this proxy cannot address. The model gets the text; the history keeps the handle. `retrieve_rounds` stays 0 on this transport for that reason — there is no round — while the restores themselves are ordinary `retrieval_events` rows, filed through the same `AppCore::retrieve` as every other surface. `proxy.retrieve_tool = "never"` turns this off too: "do not give elided content back to the model" is one operator decision, not one per transport.

Because a session whose savings cannot be counted is that same silent failure wearing a different hat, a relayed session says so: `x-kompressor-websocket: relayed` on the handshake response, and `relaying a websocket; requests are compressed, usage is not reported` in the log.

`kompressor wrap <agent>` performs all of the above for one session: it writes the variable or the config block, starts a proxy if none is listening, runs the agent, and puts the file back on exit, including after the session is killed. `kompressor unwrap codex` removes a block an interrupted run left behind.

### 12.10 Telling a live provider cache from a lapsed one

`PrefixCache::Cold` (§5.12b) says what to do with a freeze whose cache is gone; `prefixcache::PrefixTracker` is the caller that can tell.

**Zero cache reads mean "lapsed" only for a prefix that was once served.** The first request of every conversation reads nothing — the entry does not exist yet, and the provider is busy *creating* it — so a bare zero is evidence of a **new** prefix, not a cold one. Reading it as cold inverted the feature: the next turn ran with the freeze lifted and rewrote the prefix the previous turn had just written. Measured on a Claude Code subscription session: turn one reported `cache_read = 0` alongside `cache_creation = 22,363`, turn three ran with `frozen_messages = 0`, and the provider served only 14,176 of the 22,363 it held — **8,187 tokens re-read at full price, once per conversation**. Turn four returned to `frozen_messages = 2` as soon as a nonzero read arrived, which is what identified this as the per-turn cache state rather than the monotonic watermark. `Seen::was_warm` is sticky and gates both the zero rule and the TTL rule; a lapse is warm-then-zero, and the mock that tests it now serves one cached read before it stops.

**The frozen floor can describe a cache that does not exist yet** (`proxy.frozen_prefix`, default `marker`). Claude Code puts its `cache_control` breakpoint on the **final** message, so the marker-derived floor is `max(i)+1` — the whole conversation, including what arrived this turn. Measured across a 20-turn Harbor run, `frozen + opaque == messages_total` on *every single turn*: the engine was handed nothing it was allowed to move, while 19,431 tokens of conversation tail went into the cache untouched, at a lifecycle cost of $0.1417 out of a $0.4202 bill.

Those tail bytes are in no cache entry when they arrive, so compressing them invalidates nothing and the prefix in front of them still hits. `frozen_prefix = "first_seen"` freezes only as far as the provider's cache reached when this proxy first saw the conversation — `PrefixTracker::floor`, a **watermark that may fall but never rise**. Monotonic because what the provider cached is *this proxy's output*: compress message seven on turn five and the cache holds the compressed form, so freezing it on turn six would send the client's originals and miss from that byte on. Both halves are load-bearing and were verified by breaking each (`the_tail_the_provider_has_not_cached_yet_is_compressed`, with `the_marker_rule_still_freezes_the_whole_conversation` as the control).

The learned opening is namespaced by the resolved tenant, configured upstream, API family and model before it reaches `PrefixTracker`. Identical conversation text sent by different tenants, providers or models therefore cannot share cache observations, frozen-floor watermarks, retrieval counts or inline-promotion decisions. No client-specific session header is needed.

The watermark is filed from `prefix_identity`, not `prepare_inner`, for the same reason the fingerprint is: the first turn of a conversation is usually the one that declines — everything frozen, nothing to compress — so a watermark recorded inside the engine path was never recorded on the turn that defines it, and turn two filed its own higher marker count instead (`marker=3 frozen=3` where it should have read `frozen=1`).

What makes any of this safe is that the engine's output for a message does not depend on how much conversation follows it, and handles are content-addressed — so the same message compresses to the same bytes every turn. That is asserted, not assumed: `a_message_compresses_the_same_however_much_follows_it`. It is the first thing to check if a run with `first_seen` bills *more*.

**Also measured, and why the `system` block is left alone:** a captured Claude Code request is 115,063 chars — `system` 23.9%, `tools` 64.5%, `messages` 10.7% — and 72% of the tool bytes are prose `description`, not schema. Lossless gets 0.4% on the system prompt, 0.7% on tool descriptions and 1.0% on the wire-serialized schemas: ~159 tokens of 23,622, worth $0.0018 against a $0.6588 session. Aggressive reaches 70–82% but emits no handles, so it *discards* the agent's instructions outright. The walker not seeing `system` is the right call, not a gap.

**Skips are weighted by tokens, not only counted** (schema v5, `skipped_tokens_by_reason` beside `skipped_by_reason`). A count cannot rank: measured over 16 Harbor runs and 32,082 skipped messages, `below_min_chars` was 20.9% of them and tiny by construction while `opaque` was 17.9% and possibly enormous — identical in a count, opposite in what they are worth pursuing. **`opaque` is necessarily 0 and that is not a measurement**: the engine holds no text for an image, a structured tool result or an assistant turn of `tool_use` blocks, so 0 means "invisible from here", and sizing it needs instrumentation at the wire layer where the serialized bytes exist. `v_skip_reasons` reports both weights; rows written before v5 report `tokens` as 0 rather than disappearing.

**Every proxy row carries its conversation** (schema v5, `proxy_requests.conversation`, the prefix tracker's FNV-1a fingerprint as fixed-width hex). This is what makes the savings headline priceable. `tokens_in`/`tokens_out` are per-request sums, so a message compressed on turn ten is counted again on every turn through the end of the session and the total over-counts by roughly how long a message survives — §20 recorded this as deferred for want of a conversation identity the wire does not offer, and the tracker's fingerprint is that identity. `v_conversations` groups by it: `cache_write` summed there is the unique prefix size, because each token is written exactly once. Hex rather than the raw `u64` because SQLite integers are signed and half the fingerprint space would round-trip as a negative number. The key is the head of the conversation **whether or not anything is frozen**: a first version took the frozen head and so filed nothing for OpenAI, whose floor is always 0 — 66 of 66 null rows on a gpt-5.5 run, i.e. the accounting was absent on the provider where the engine removes 61% of what it sees.

**A discard must not cache bytes a later turn will rewrite.** Discarding forwards the client's *original* bytes, so the provider caches those; a later turn that does compress the same messages sends different bytes and the prefix misses from there on. The flip is raw-to-compressed, so suppressing the reverse would not help — what has to stop is caching raw bytes for messages a later turn will rewrite. Both guards in `prepare_inner` therefore yield once `PrefixTracker::has_been_cached` is true: before anything is cached a discard is free and the guard it protects is worth keeping ("a body that grew is a body we should never have touched"), and after, a marginally larger body costs a few hundred tokens once while a missed prefix costs the re-read of everything following the first changed byte. Measured on a 39-request Opus run: turns four and five discarded, turn six compressed, and the cache fell back to turn four's prefix — losing exactly `cw[4] + cw[5]` = 8,299 tokens, 11.1% of everything that run wrote.

**A line-number column must not change what survives.** `gutter` lifts the column, routes the body, and re-attaches the numbers to the lines that came back, and the lift is then arbitrated on output size. For every type but one that is right. It is wrong for source: a numbered file is two columns, so a row sampler claims the column-on copy, keeps the head and tail and drops the middle, while the lifted copy keeps every signature and elides the bodies. Both carry retrievable handles, so size cannot separate them — and an agent reading source can act on a file of signatures and cannot act on its first and last twenty lines. Measured on 400 numbered lines of this crate's own `crusher.rs`: **5,491 bytes as a row sample against 13,197 as code**, so size chose the sampler every time and `Read` output — how Claude Code delivers every file it opens — was routed to row-dropping. The arbitration now refuses that when the *stripped body* detects as source, which costs ratio deliberately (30.0% against 70.9% on that file, versus 29.4% for the same file unnumbered). Keyed on the body's own type rather than on either side's `Kind`, because a kind says what a payload was and never how it was reduced — a row-sampled result still reports `SourceCode` when the numbered copy was detected as source. The lift itself also runs before the rest of the detector chain now, not only inside the config branch, since a gutter defeats every rule anchored at the start of a line.

**The prefix-monotonic invariant, and the one transform that breaks it.** `crossturn` states the rule and every other decision respects it: a message is matched only against strictly earlier ones, so the output of `0..k` is byte-identical whether or not `k+1` exists, and appending a turn never rewrites an earlier turn. That is what lets the proxy compress history and still hit the provider's cache — demonstrated at scale on OpenAI, where the floor is zero, the engine rewrites all history every turn, and the cache still served 95.4% (median 97.2% over 50 turns).

**Stale-read is the exception**: a later write to the same path replaces the read before it, so appending a turn *does* rewrite an earlier one. The two features cannot both be had, and `frozen_prefix = "first_seen"` therefore withholds `enable_stale_read`. The economics decide which goes: at Opus rates, re-writing a 20,000-token suffix to remove a 2,000-token stale read needs ~125 further turns to break even, against sessions that run 20 to 70. `stale_reads_are_the_one_transform_that_rewrites_what_came_before` asserts both directions, so whoever makes the transform causal finds the test rather than the bill.

Note the reach: the stale replacement handles `MessageContent::Text` only, so it cannot fire for Anthropic tool results that walk to `Blocks`. The gate is insurance for the shapes where it can (OpenAI string content, single-block messages), **not** an explanation of the cache churn measured on the 2026-09-27 `first_seen` run — that break, on the one turn where `refs_emitted` went 0→1, is still unexplained and needs the two consecutive request bodies diffed (`log.payloads`) rather than another hypothesis.

**Usage counters are levels within a round and increments across rounds.** Anthropic reports the full input side twice — in `message_start` and again in `message_delta` — both describing the one call, so the relay folds a round's frames with `turn::max_usage` and only sums *between* rounds with `turn::add_usage`. Folding within a round with `add_usage` doubled every input counter: measured on the 2026-09-27 Anthropic smoke, 1,978,478 cache-read tokens filed where the client reading the same responses saw 989,239, a ratio of exactly 2.0000, while `output_tokens` stayed correct because `message_start` carries only a placeholder for it. Halving the input side reproduced the provider's bill to the cent. The maximum rather than last-frame-wins, because the older wire shape omits input counters from `message_delta` entirely. Every `message_delta` fixture in the crate was that older shape, so the suite agreed with the bug for as long as it existed; `a_streamed_round_counts_each_input_token_once` now drives the real two-frame stream through the relay.

**The asymmetry decides the design.** Wrongly `Cold` rewrites a live prefix and the provider re-reads everything after the first changed byte at full price — on the workloads measured here 90–100% of the billed prompt is cache reads, so that turns the cheapest part of a request into the most expensive. Wrongly `Warm` leaves a saving unclaimed and breaks nothing. So this never infers cold; it reports cold only on evidence already seen: the provider telling us, for this exact prefix, that it read nothing from cache.

**The key is the conversation's head, not a session header and not the whole prefix.** `x-claude-code-session-id`, `prompt_cache_key` and whatever the next client invents are client-specific, and a proxy that works only for clients it has heard of is the silent failure §14.1 is organised against.

Hashing the *whole* frozen prefix sounds more honest — it is what the provider caches on — and is useless, because an agent advances its cache breakpoint as the conversation extends. Measured on a Claude Code run: `frozen_messages` went 0, 3, 4, 5, 7 … 26 across 24 requests, **23 distinct values**, so every turn asked about a key no turn had filed evidence under. The tracker was correct and permanently empty. The provider's cache does not work that way either: a longer prefix still hits cache for its common head.

Hashing a *fixed selection* of opening messages fails the other way: it is stable but it does not DISCRIMINATE. An agent harness opens every conversation with the same preamble, so every conversation gets the same key. Measured on a 20-task Harbor run, all 637 requests collapsed into **five** keys — and those same five reappeared in a later run with different tasks, which is the tell: a key that repeats across runs is naming the client, not the conversation. Adding the first `user` message helped and was still not enough, because for some clients that turn is boilerplate too. The earlier claim that two conversations opening identically may share a key *because they share the cached prefix anyway* is true only of the cache half; under `frozen_prefix = first_seen` they also share a **watermark**, and since the floor may fall but never rise, a second conversation opening at 0 drags a mid-flight one's floor down and its next turn rewrites a prefix the provider is holding.

So the opening is **learned rather than derived**. `prefixcache::identity_chain` builds a rolling chain in which `chain[i]` identifies messages `0..=i` exactly; `PrefixTracker::identify` finds the deepest registered prefix a request extends, and a request matching nothing registers its own whole message list as a new conversation. Later turns still match because appending a turn extends the opening rather than changing it, and two conversations are told apart at the first message where they actually differ rather than at a depth guessed in advance. Capped at `IDENTITY_DEPTH` messages, which bounds both the hashing per request and how much boilerplate two conversations may share before they are separated. Measured after the change: four tasks that had produced one key produced four, and a 20-task run produced one identity per trajectory.

FNV-1a rather than `DefaultHasher`, whose per-process seed would make two proxies — or one restarted mid-session — disagree about which conversation a request belongs to.

**The identity is stamped outside `prepare_inner`.** That function has ten ways to decline, and the case this feature exists for — a request frozen end to end, so nothing compresses — takes the passthrough path. Passthrough carried no fingerprint, so the prefix was never recorded, the provider's answer never learned, and the state stayed `Warm` for good: `tracked()` read 0 after three turns that should have filed one entry.

`proxy.prefix_cache_ttl` adds an age rule and ships at `0s`, off. A TTL is a guess about someone else's eviction policy, and guessing cold is the expensive mistake.

Two things that were wrong on the way in, both caught by tests rather than review:

- The fingerprint read `MessageContent::Text` only, so an Anthropic `tool_result` — which arrives as `Blocks` — hashed to nothing and every prefix stayed `Warm`. The end-to-end test passed anyway, because it asserted the second turn was *not larger*; `<=` is true when nothing happens. Asserting *strictly smaller* is what exposed it.
- A request can reach `Recorder::finish` more than once (an abandoned stream, an error path), and the later call carries no usage. `None` overwrote the provider's own `Some(0)` and the next turn saw nothing known. `None` now means "no evidence" in both directions: it neither establishes nor erases.

### 12.6 Retrieve tool: injection and server-side execution

**What the description says about markers.** It names the three shapes a handle
can appear in — an omission, `[... 412 matches omitted [ref:k_…]]`; an elided
body, `# [... 77 body lines omitted from join_revisions; retrieve: ref:k_…]`;
and a fold, `connection refused [×47 ref:k_…]`. It used to name only the first,
then the first two.

**What the model actually asks for.** Counting settles which shape works.
Across eleven runs with any retrieval — 93 fetches, every one a hit, no misses
and no errors — **37 of the 44 distinct fragments ever retrieved were code
bodies**, named by the shape the description did not mention. Censusing three
runs for what the model was shown:

| run | retrievals | markers | body | omission | fold |
|---|---:|---:|---:|---:|---:|
| mailman smoke | 15 | 40 | 9 (23%) | 4 (10%) | 17 (43%) |
| set9 tasks | 0 | 34 | 0 | 2 (6%) | 32 (94%) |
| html/json tasks | 0 | 274 | 11 (4%) | 3 (1%) | 241 (88%) |

Two things follow, and both are implemented. The body shape is documented,
because it is the only one that says *what* is missing by name — which is what
lets a model decide whether it needs it. And the fold shape stops carrying a
handle it cannot justify: in the html/json run those 241 folds held 64 bytes
each, 158 of them a single duplicate line, and not one was fetched. A model that
spends a round trip on two of those learns that markers are not worth reading,
which is the wrong lesson for the fourteen that were. See §7.4 for the rule and
`fold_handles_identical_repeats` for the opt-out.

**Why the elided body is the shape that matters.** In a measured failure the
agent was reading a library to decide how a leaderboard aggregates scores. The
bodies of `join_revisions` (77 lines, docstring included — it states the
conflict-resolution rules), `_aggregate_and_pivot` and `_compute_mean_public_private`
each came back as a marker over a filler. It retrieved none of them, asserted
the semantics and answered wrong. The filler used to be `pass`, which reads as a
body that deliberately does nothing — a stub that parses *and* looks finished.
It is now `...`, and the description says in as many words never to reason about
an elided body without retrieving it first.

**Injection.** If the outgoing request contains at least one `ref:k_` marker (or `proxy.retrieve_tool = "always"`), the walker appends a tool definition:

- OpenAI Chat/Responses: `{type:"function", function:{name:"optscale_kompressor_retrieve", description: …, parameters:{type:"object", properties:{ref:{type:"string", pattern:"^k_[A-Za-z0-9_-]{12}$"}, range:{…}}, required:["ref"]}}}` (Responses uses the flat `{type:"function", name, parameters}` shape).
- Anthropic: `{name:"optscale_kompressor_retrieve", description, input_schema:{…}}`.
- If the client already sent a tool named `optscale_kompressor_retrieve` (it runs our MCP itself), nothing is injected and execution mode is forced to `client`.
- `tool_choice` is left untouched; the model decides. The system prompt is never modified (the tool description carries the instruction — the MCP `instructions` text, adapted).

**The `range` schema is a `oneOf`, not four optional siblings.** It used to be
four optional sibling objects — `lines`, `chars`, `json_pointer`, `grep` — with
"give exactly one of these" stated only in prose. Asked to narrow, a model filled
in all four:

```json
{"ref":"k_9Z9xkNVTNAy3","range":{"lines":{"start":1,"end":80},"chars":{"start":0,"end":0},
 "json_pointer":{"pointer":""},"grep":{"pattern":"","context":0}}}
```

`RetrieveRange` is an externally tagged enum with `deny_unknown_fields`, so a
four-key object deserialized to nothing, the range was dropped, and the *whole*
fragment came back — the expensive answer, logged at `debug!`. Across two paid
runs every retrieval reported `ranged = 0`; the narrowing path had never once
fired in production. Two changes, because either alone leaves the other half
open:

- the schema carries `maxProperties: 1`, `minProperties: 1`,
  `additionalProperties: false` and a `oneOf` over the four — `maxProperties` is
  what a decoder can enforce *while* generating, `oneOf` states the rule for
  anything validating afterwards;
- `RetrieveRange::parse_loose` salvages what arrives anyway. Members that are
  plainly filler — a zero-width char window, an empty pointer, an empty pattern —
  are dropped, and if one substantive variant remains it is used. On the payload
  above that leaves `lines`, which is what the model meant. A genuinely ambiguous
  request resolves by a fixed priority (`lines`, `chars`, `json_pointer`, `grep`);
  one with nothing substantive left sends the fragment whole, and says so at
  `warn!` rather than `debug!`.

The MCP surface was never affected: its schema is derived from the same enum by
`schemars`, which emits the `oneOf` automatically.

**Execution** (`proxy.retrieve_execution = "server" | "client"`, default `server`):

- `client`: the tool call reaches the client as any other tool call; the client resolves it via our MCP/HTTP API. Zero interception logic; retrieval attribution via the API key.
- `server`: the proxy runs an **agent loop** bounded by `proxy.retrieve_max_rounds` (default 3) and `proxy.retrieve_max_tokens_per_request` (default 16,384 injected tokens). Non-streaming:
  1. Upstream response parsed; if the assistant turn contains only `optscale_kompressor_retrieve` tool calls (any other tool call ⇒ hand the whole response to the client unchanged; mixed calls ⇒ same, because we cannot partially satisfy a turn), execute each: store lookup (write-through cache → store), range slicing, `retrieval_events` rows, `retrieve_tokens_injected += estimate_tokens(returned)`.
  2. Append to the request: the assistant message (with its tool calls) and one tool result per call (`role:"tool"` / `function_call_output` / `tool_result` block, per family). For Anthropic, tool results go in one `user` message as blocks.
  3. Re-send upstream (the compressed base prompt is unchanged, so the provider's cache prefix holds); repeat until a round produces a normal answer or the cap trips. On the cap, the next `optscale_kompressor_retrieve` call is answered with a tool result `{found:false, error:"retrieval budget exhausted"}`. A refusal ends the *spending*, not the loop: the round after it is classified like any other. A model that answers is relayed; a model that calls `optscale_kompressor_retrieve` again is refused again, for at most `MAX_REFUSAL_ROUNDS` (2) rounds, after which the request ends as a 502 `kompressor_retrieve_loop`. **The client is never handed a turn containing `optscale_kompressor_retrieve`** — it never declared the tool, so such a turn is not something it can act on. An earlier version relayed the post-refusal round unconditionally and a model that kept asking put the call straight into codex's hands, which answers `unsupported call: optscale_kompressor_retrieve` and loses the turn. Measured on the eight-language bug hunt (§18.5a): three bodies fetched, refusal, and the turn thrown away.
  4. The final response is returned to the client with `usage` **summed across rounds** (so the client sees what it paid) and the intermediate assistant/tool round hidden (the client never asked for that tool). A header `X-Kompressor-Retrieve-Rounds: n` reports what happened.
- **A fragment fetched back in two different requests stops being elided.** The round that answers a retrieval re-sends the whole prompt upstream, and the fetched bytes live only in that hidden round — they never reach the client's history, so the next turn re-compresses the same content to the same handle and the model asks again. Measured before this existed (2026-10-02, 56 requests): 12 retrieval events over only **5 distinct fragments**, the same three handles fetched on three consecutive turns, each spending the full `retrieve_max_rounds` budget; the four retrieve-round requests carried 69K/77K/87K/130K prompt tokens against ~31K for their neighbours — roughly **274,000 prompt tokens to deliver 3,815 tokens of fragment**, about 9% of that run's billed prompt. `PrefixTracker::record_retrieved` files each hit against `(conversation, handle)` with the request it arrived in, and `RequestOptions::keep_inline` carries the promoted handles back into the engine, which compresses the message, sees that a handle it would mint is on that list, rolls the compression back and ships the message whole (`skipped_by_reason: retrieved_before`).

  **Two requests, not one, and that threshold is load-bearing.** The loop's signature is the same handle asked for across *different* turns; a model narrowing twice inside one request is reading, not looping, and `keep_inline` could not help it anyway because it applies from the next request onward. Acting on the first fetch was tried and measured (2026-10-02, 39 requests): **all eleven** fetched handles turned out to be one-offs, and keeping them inline anyway cost 503,355 tokens — **24% of everything the engine looked at** — for nothing, dropping removal from 53.5% to 43.6%. Waiting for a second request costs one extra round on a handle that really is looping and costs nothing at all on the common case.

  **A turn that asks about a marker gets the content, without a retrieve round.** This is the recovery path for a turn that never calls the tool, and the test it uses is shared with the websocket relay, where no tool can be offered at all (§12.5b). On the way *in*, the proxy scans the request for `ref:k_…` markers, and for each one this conversation holds it reads the fragment (`AppCore::peek`, which deliberately does not touch the retention clock or file a `retrieval_events` row, because the proxy looking at its own bytes is not the model asking) and compares it with the newest user text. Two distinct words of four characters or more in common promote the handle, after which the ordinary keep-inline path ships the message whole — cost guard included.

  **On HTTP this path fires essentially never, and that is structural rather
  than a defect.** Measured over the 89-task run of 2026-10-12, which made 180
  served retrievals: the proactive test would have predicted **0** of them, and
  the reason was the same every time — the marker was not in the request. The
  handle appeared as a `ref:k_` marker in what the proxy sent *upstream* on
  180 of 180, and in the client's own request, in any form, on **0 of 180**.

  The sequence explains itself once seen. The proxy mints the handle while
  compressing *this* request, the model meets it in the reply to that same
  request and calls `optscale_kompressor_retrieve` immediately. There is no earlier turn
  in which anything could have been predicted, and the marker reaches an
  incoming request only when the model has previously written one into its own
  visible output — which a model holding a working tool has no reason to do.

  **So porting the relay's piggyback to HTTP would buy nothing**, which is
  worth recording because the cost it appeared to address is real: 110 of 2,233
  requests ran a retrieve loop over 181 rounds, billing 74,338 prompt tokens on
  average against 40,594 without one — 1.8x, about $3.4 of that run's $98.75.
  Piggyback appends to the *next* frame and has exactly the same shape as this
  path. It works on the relay precisely because no tool can be offered there,
  so an unresolved handle survives across frames until the mention test fires;
  give the model a tool and it asks on the turn the handle is minted.

  The only way to pre-empt a first fetch is to decide at compression time not
  to elide that fragment, which means testing it against the current question —
  the query-dependent output §18.4 forbids, because an agent re-sends its
  history every turn under a different latest question and query-dependent
  bytes rewrite the prefix the provider is caching. Prefix churn costs more
  than 3.4%.

  What remains is bounded and already addressed. Of those 180 retrievals, 94
  were **first** fetches of 94 distinct handles — nothing could have predicted
  them — and the other 86 were repeats, which is the case `retrieved_before`
  exists for and which caught 617 messages and 1,008,774 tokens on the same
  run. Lowering its two-fetch threshold to one is the obvious remaining idea
  and the measurement argues against it twice: 52% of this run's retrievals
  were one-offs, and the 2026-10-02 trial of exactly that cost 503,355 tokens
  for nothing and dropped removal from 53.5% to 43.6%.

  **The retrieve round is the price of letting the model fetch back what
  compression removed, and on this evidence it is a fair one.**

  **A ceiling on the whole inline set, because the per-message guard prices the wrong thing.** `keeping_it_inline_is_worth_it` asks whether *this* message is worth keeping whole, comparing one turn's forfeit against what it protects. It then commits to every remaining turn, and it never sees that multiplier. Measured across four runs of one terminal-bench task: run D kept two messages of 3,958 tokens inline from turn 7 to turn 40 to protect three fragments totalling 834 tokens fetched twice each. At the moment of decision that is **2.4x** forfeit-to-protected, comfortably inside the 32x threshold; over the 34 turns it actually applied to it is **80.7x**, two and a half times outside it — and the guard still had roughly **13x headroom**, so nothing on that path would ever have stopped it. The trend said the same thing: `retrieved_before` across the four runs went 0, 0, 55,040, 134,572 tokens, and backing it out leaves removal at 48.9%, 50.3%, 49.2% — the compressor itself flat, the headline falling only because compression was being declined.

  So `proxy.keep_inline_max_percent` (default 40) bounds the set rather than the decision: the forfeit for one request cannot exceed that share of what the engine looked at, however many individually-reasonable promotions produced it. Two details are load-bearing. It is priced by **the message that would be shipped whole, not the fragment inside it** — those differ by nearly five times in the run above, and charging for what is protected rather than what is given up would be the same category of error in a second dimension. And admission is **in promotion order and never revoked**: the set grows, the context it is measured against grows, so a fragment admitted once stays admitted. Evicting one would put a handle back where inline text had been, rewrite the prefix, and pay the §12.10 price on everything after it to save a fraction of it. Calibrated like `KEEP_INLINE_MAX_FORFEIT`: the healthy observed peaks are 17.9% and 26.9% and must not be blocked, the runaway the per-message guard would still pass is about 79% and must be, 40 sits between with room either side. It has never fired on measured traffic, which is what a bound against a failure not yet seen should look like — not evidence that it works, only that it is not in the way.

  **A promotion pays a one-off cache break, and that is now priced.** Turning a
  marker the provider has already cached back into full text rewrites the prompt
  at that message, so everything after it is re-read at the fresh rate — once.
  The per-message guard above cannot see this: it prices the compression
  forfeited on each later turn and never the rewrite itself. Measured on
  2026-10-07 with both bodies captured: a **175-token** fragment was promoted
  into a message sitting **17% into the prompt**, `cache_read` fell
  18,816 → 12,672, and **6,144 tokens** were re-read. Handles in the outgoing
  conversation went 11 → 7 on that turn, which is the promotion. Breaking even
  needed the model to fetch that fragment **35 more times**.

  So `affordable_inline` now refuses a promotion whose break exceeds
  `KEEP_INLINE_BREAK_PAYBACK_FETCHES` (8) times the fragment it protects, and
  says so at `warn` because the consequence is that the model may ask again.
  **The constant is a bet, not a measurement**, and on this path it remains one.
  The reasoning was that once a fragment is promoted nothing ever reveals
  whether it would have been wanted again, so the saving side is unobservable by
  construction. That holds for *promotion* and not for *refusal*: a refused
  fragment announces itself once per round until it is promoted, which is what
  the engine's own rule now counts (below). **This path does not count it** —
  it promotes on the query heuristic rather than on demand, so it has no demand
  to weigh and keeps the fixed bet. The two paths therefore price the same
  break differently, deliberately and not by oversight.

  Eight
  refuses the measured case by a wide margin and leaves cheap promotions alone —
  a message near the end of the prompt has almost nothing behind it, so its
  break is small whatever the fragment is worth. A fragment already shipping
  inline has no marker to replace, no break, and is never re-judged; re-deciding
  it each turn would be the very prefix churn this exists to prevent.

  **The demand-counting path has now been seen doing this in production, and it
  came out ahead.** Harbor run 2026-10-12 09:54, three cache losses of which
  `cache-loss-diff.py` attributed two to the provider and one to us:

  ```
  17,792 -> 16,768  lost 1,024  gap 14.2s
    tools identical True   instructions identical True
    messages 60 -> 67, first differing index: 48
    -> OURS
       message 48: 404 -> 4,681 chars
       PROMOTION (markers 1 -> 0: a fragment came back inline)
       k__zWTw3yvYTxc was fetched 2x on turns [27, 30]
  ```

  The store has that fragment at 4,246 bytes and `retrieval_events` has the two
  fetches, 29 seconds apart, 1,047 tokens returned each. So the promotion cost
  **1,024 tokens** re-read at the fresh rate — about 922 tokens of real cost at
  the measured 10× cache discount — against 1,047 saved for every fetch it
  prevents. **One** avoided fetch pays for it, where the 2026-10-07 case needed
  thirty-five. The difference is not luck: that one promoted on the query
  heuristic, which has no demand to weigh, while this one promoted on a second
  fetch and so had the demand in hand. The two paths pricing the same break
  differently is the decision recorded above, and this is the first measurement
  of the side that counts.

  It is also the first time the diff tool has answered `OURS` on a real run
  rather than in a test, which exercises the part of it that was wrong once
  already: the direction is read by **counting `ref:k_` markers** (1 → 0 here),
  not by looking for the omission wording, and an earlier version keyed on
  `omitted from` — text only the code compressor writes — and so filed a real
  promotion as "other".

  **Only markers present in the request are considered, and promotion is sticky.** Both bounds are load-bearing. A tracker of every compression the process has made leaks across conversations: in headroom's equivalent feature a 127-character teammate message reached the model as 27,722 characters of another agent's tool output, and the fix was the same restriction. A fragment this conversation never received was never context it lost. Stickiness matters because a query-driven expansion that came and went with each turn's wording would rewrite the prefix back and forth; once wanted, a fragment stays inline exactly as a twice-fetched one does. Note what carries the marker back: the client's own tool output is stored uncompressed, so a `ref:k_…` in a request was written by the *model* — which is itself the signal that it met a marker it could not resolve.

  **A cost guard bounds what a promotion may forfeit.** The rollback is message-granular while the loop is fragment-granular, so keeping a fetched fragment inline gives up that whole message's compression on every later turn. Usually a good trade — measured on a 48-request run, three fragments totalling 700 tokens held two messages inline at ~3,960 tokens a turn against retrieve rounds costing ~20,000 a turn — but not when the message is enormous and the fragment tiny: inlining a 50,000-token log to protect 233 tokens adds more to every prompt than the round it avoids. The engine therefore keeps a message inline only while the compression it forfeits is at most `KEEP_INLINE_MAX_FORFEIT` times the fetched fragments it protects, and warns when it refuses, because the consequence is that the model may spend another round on that fragment.

  **The multiple is derived, and for a long time it was guessed.** It was 32, chosen to sit between an observed healthy ratio of ~8.5 and a pathological ~215, with a note saying the engine could not do better locally because "the real denominator is what a retrieve round costs, which is the whole prompt, and the engine never sees it". The reasoning was sound; the conclusion was avoidable. The denominator is not the prompt — it is the *rate the prompt is billed at*. Holding a fragment inline pays `size` every turn, the first fresh and the rest from cache; re-fetching pays `fetched` every turn at the fresh rate. They cross where `size / fetched == fresh_rate / cached_rate`, so **the crossover ratio is the cache discount**, whatever that happens to be.

  Solved by least squares against seven live terminal-bench runs — cost, fresh, cached and output tokens, three unknowns and seven observations — the provider's rates come out at **$5.00, $0.50 and $30.00 per MTok, fitting every run to 0.0%**. A discount of exactly 10, so the threshold is 10.

  **The same discount holds on Anthropic, and there is a second term there.** Fitted against four `claude-opus-5` runs under Anthropic's published structure (read = 0.1x input, write = 1.25x input), so two free parameters against four observations rather than an exactly-determined system: **input $5.00, cache read $0.50, cache write $6.25, output $25.00 per MTok, every run to 0.00%**. Input and cache read are the same numbers as OpenAI and the discount is again exactly 10, so `KEEP_INLINE_MAX_FORFEIT` needs no per-provider value.

  Cache *writes* are the term OpenAI does not have, and they are not small: on the 2026-10-07 run they were 16.7% of total cost, at 12.5x the price of a read. This changes what a broken prefix costs. A rewrite loses the reads it would have had **and** pays to write the cache again, and the second part is the larger one — for the 291 tokens that run lost, $0.00131 in forfeited reads against $0.00182 to rewrite. `cache lost` used to report only the read side, so on Anthropic it understated a rewrite by about 2.4x. It now carries the write side as well: each loss records what the losing turn wrote back, and the report adds a line wherever the provider bills writes. The span priced is `lost` rather than that turn's whole write, because a write also covers content that was new regardless — `lost` is the marginal cost and the turn's write is the upper bound. On OpenAI the field comes back NULL and the line is suppressed, which is the honest rendering: the read side there is the whole cost, and a line reading "0 rewritten" would invite a reader to wonder what went uncounted. Every field ratio measured sits under it: 8.5 on the original case, and 3.0, 2.4 and 3.4 on three later runs holding one, two and three messages inline. The bound now admits everything observed to work and refuses only what does not pay, where 32 would have approved decisions losing money threefold.

  **Position was missing from this rule, and it is the term that dominates.** `KEEP_INLINE_MAX_FORFEIT` prices the *recurring* cost of holding a message whole and says nothing about the *one-off* rewrite that promoting it causes — which depends entirely on where the message sits. Measured 2026-10-08 on `claude-opus-5`: a 309-token fragment kept inline at 15.9% of the prompt cost **46,379 tokens of cache — $0.21 in forfeited reads and $0.29 to write it again, 17.6% of a $2.83 run** — to avoid about $0.04 of retrieve rounds. Twelve times underwater, admitted because 309 tokens comfortably justified what that one message gave up.

  The proxy's promotion path had priced the break since §12.6; **the engine's had not**, and this is the path the loss came through.

  The first fix compared `break_cost` against `protected × 8` — a prompt against a fragment — and that was wrong in a way the next run exposed. Fragment size has nothing to do with how often a fragment is wanted, so the bound moved with whatever size the fragment happened to be: for the ~300-token fragments these runs produce it allowed about one round's worth of break while claiming eight, and for a 4,500-token one it allowed twelve. Measured 2026-10-08 on OpenAI, it refused seventeen promotions whose handles were then fetched four, four, three and two more times — **nine avoidable rounds, $0.35, against $0.25 to promote**. Better than the 46,379-token failure it replaced, and still the wrong quantity.

  **What repays a rewrite is the number of future fetches it avoids, and refusing is what makes that observable.** A promoted fragment is never asked for again, so nothing reveals what it would have cost — the reason this was called unobservable. A *refused* one announces itself, once per round, until it is promoted. `PrefixTracker` counts fetches per handle per conversation and the engine weighs them:

  ```text
  break   = behind × (fresh − cached)
  a round = whole  × cached
  promote once   fetches ≥ (behind / whole) × (fresh − cached) / cached
  ```

  The prompt size cancels and only the rates remain. **The denominator is the blended rate, not the cached one**, and getting that wrong is what the first version of this shipped: written as `cached` it put the multiple at a flat 9, where the traffic made it 6.7, so the rule asked 35% more demands than the economics required. Measured 2026-10-08: it refused 72 promotions in one run, the bar sitting at 7–8 while the fragment it protected was fetched five times, and that conservatism cost $0.19 of retrieve rounds on a $2.55 run.

  A round pays the fresh rate on whatever part of the prompt is *not* cached, so the multiple moves with the hit rate — **5.0 at 91.3%, 6.7 at 96.1%, 7.3 at 97.5%**, reaching 9 only at a hit rate of 100%. That is a property of the traffic, so it is measured rather than fixed: `PrefixTracker` sums billed prompt tokens and the cached part of them across the process, and `fetches_to_repay_a_break` computes the bar per request. Three constants in this rule have now each been right for one run and wrong for the next; this one is derived from what the provider reports.

  Where nothing has been observed the rate reads as 1000 — fully cached — which recovers the old 9 and is the most demanding reading. `RequestOptions::default` is hand-written for it, because `#[derive(Default)]` gives zero, and zero does not mean "unknown": it means "nothing is ever cached", under which a rewrite looks free and a single demand promotes. That is the direction that cost 46,379 tokens in one turn.

  Both field cases come out right — the Anthropic fragment asked for twice at 84% back is refused, the OpenAI one asked for four times is promoted. Anthropic also bills the rewrite, which puts its true figure above this one, so there too the bias is towards refusing.

  **A promotion is taken once, because it is paid for once.** The bar moves in both its terms — the prompt grows behind the message, and the measured hit rate converges as a run proceeds — so re-deciding each turn lets a fragment be promoted and then demoted, and the demotion rewrites the prefix a second time. The first run of the adaptive rule did exactly that: 14 messages promoted while the running rate read 82%, and one re-elided once it reached 94%, for **8,192 tokens** — a loss whose byte diff reads backwards from every other, full text before and a marker after. The engine now reports what it shipped whole in `CompressStats::kept_inline_refs`, and `PrefixTracker` pins those fragments at `u32::MAX` demands so no later bar can overtake them. This is the same stickiness §12.6 already required of the query-driven path, which had it from the start and for the same reason.

  The relayed websocket is exempt and passes `u32::MAX` demands. There a refused promotion is not a cost — it is the loss of content the model was already handed, since that transport has no tool to ask again with.

  One assumption remains, and it favours keeping inline: the crossover above assumes the fragment would otherwise be re-fetched on *every* remaining turn. A fragment wanted only twice more is cheaper to re-fetch than to hold, and a ratio of 10 still passes it. That frequency is unobservable by construction — once promoted, nothing reveals whether a fragment was wanted again — so 10 is a ceiling under the generous reading, not a break-even under the true one.

  **A promotion belongs to a conversation, and conversations are identified by their opening text.** Two sessions that open with byte-identical messages are one conversation to the tracker, so a handle promoted in the first stops being elided in the second. That is usually what you want — it is the same content — but it is visible wherever independent sessions share an opening by construction: the e2e suite drove seven retrieve-loop scenarios from one shared payload, the second scenario's fetch promoted the handle, and scenarios three onward found nothing left to retrieve. Six checks failed at once on a proxy behaving exactly as designed. The fixture now tags each scenario's payload so each is its own conversation.

  **This deliberately breaks prefix-monotonicity, once per promoted fragment** — a message elided on turn five returns whole on turn seven, so the provider re-reads from there. That cost is paid once, against a round that was being paid every turn the content was wanted, and is free on OpenAI, which does not bill cache writes. Measured across three runs since, no cache regression against a growing prefix has yet been observed. The map is bounded at 64 handles per conversation and 1,024 conversations, because it is keyed by caller-controlled input.
- Miss/expired: the tool result says `{found:false, expired:true|false, hint:"the elided content is no longer cached; re-run the command or re-read the file"}` — never invented data.
- Hit with an empty range: a narrowed retrieval that selects nothing is still a *hit* — the handle resolved — so it returns `{found:true, content:"", hint:…}` where the hint names how the range missed and what to try instead (a line window is told the fragment's real line count; a pattern is told the match is literal, not a regular expression). Without it the model reads a successful empty answer, learns nothing, and spends another round: measured at two of four rounds on one session, which then hit the round cap and lost an answer.

**Streaming** (SSE) with `server` execution — the case that gives this crate its shape, because a retrieve loop that runs only on buffered responses is worse than no loop at all — is handled by a per-family **relay state machine**:

```
state Buffering: read upstream SSE events into a buffer, forwarding nothing, until one of:
   (a) a text/content delta arrives                       ⇒ state Passthrough (flush buffer, then relay live)
   (b) a tool-call start names something ≠ optscale_kompressor_retrieve ⇒ state Passthrough
   (c) a tool-call start names optscale_kompressor_retrieve         ⇒ state Intercepting
   (d) the stream ends with no content (e.g. empty)        ⇒ flush, done
   buffer bounded by proxy.stream_buffer_bytes (256 KiB) ⇒ overflow ⇒ Passthrough
state Intercepting: consume the whole assistant turn (all tool-call deltas until message end / finish_reason
   tool_calls); if any non-retrieve call appeared ⇒ hand the buffered turn to the client with our own call
   elided and the turn-level frames rewritten (Passthrough, §12.9); else execute retrievals, build the next
   request, open a new upstream stream, and re-enter Buffering (round += 1). Bounded by
   proxy.stream_intercept_bytes (1 MiB) and proxy.stream_intercept_timeout (90 s); reaching either elides
   the same way. Message-start events for Anthropic are emitted once, with the id of the
   final round; usage deltas are summed and emitted on the final message_delta/final chunk.
state Passthrough: relay bytes as they arrive (zero-copy frames), rewrite nothing except the final usage
   frame when rounds > 0.
```

Because tool-call **names** arrive in the first delta of a tool call in all three families (OpenAI `tool_calls[0].function.name`, Responses `response.output_item.added` with `name`, Anthropic `content_block_start` with `name`), the buffering window is normally a few frames; time-to-first-byte increases only for requests that actually retrieve. Keep-alive comments (`: ping`) are sent to the client while intercepting if the upstream round takes longer than `proxy.stream_keepalive` (10 s) so client SDK read timeouts do not fire.

### 12.7 Header and body policy

- Forward the client's `Authorization` / `x-api-key` / `anthropic-version` / `anthropic-beta` / `OpenAI-*` headers unless the upstream config replaces auth. Strip hop-by-hop headers and `Content-Length` (re-computed), and **replace** `Accept-Encoding` with `identity`: we need plaintext to parse, and the HTTP client is built without any decompression feature, so asking for `identity` is what guarantees a body the retrieve loop can read rather than something we would have to decompress ourselves. The cost is that a client's own `Accept-Encoding` never reaches the provider, which is a visible difference for a subscription CLI — see `authmode` below.
- **Recognise a stream the client asked for even when the upstream does not label it.** `is_event_stream` decides whether the SSE relay runs at all, and a response that is not recognised takes the buffered path — where the retrieve loop cannot intercept anything. chatgpt.com returns the Responses stream with **no `Content-Type` header**, so requiring it meant every subscription stream skipped the relay: the model's `optscale_kompressor_retrieve` call was relayed to the client, which rejected it, and the model answered without the fragment. An absent type is therefore read as the stream the request asked for; a type that is present is believed over what we asked, because an upstream answering `application/json` to a streamed request is answering something else and running the relay on it would find no frames and hand the client an empty body.
- **A sentence before a tool call does not end the turn.** The relay decides whether a streamed turn is ours by watching what the model emits, and it used to treat *content* exactly like a call to one of the client's own tools: proof the turn was not ours, hand the buffer over, relay the rest untouched. A reasoning model routinely says one sentence before it calls a tool, and every `optscale_kompressor_retrieve` call after that sentence then reached a client that never declared the tool — `unsupported call`, no fragment, nothing fetched. Measured against codex: `gpt-5.5` leaked thirteen calls in one session and retrieved nothing, while the same commit on `gpt-6-sol`, which calls without narrating, retrieved twenty. Content is now bounded rather than fatal: it is held while the turn may still turn out to be ours, up to `proxy.stream_content_grace_bytes`, and **only on turns where the retrieve tool was actually injected** — a turn that could not retrieve has nothing to wait for and streams as immediately as it ever did. `OtherCall` still ends the turn at once, because a tool the client declared really is its own. `0` restores the old rule.

  **The bound counted the wrong thing, and the bug it was built for came back on the same model.** `content_bytes` summed `frame.raw().len()` — the whole SSE frame, mostly JSON envelope — so 8 KiB bought roughly thirty deltas rather than 8 KiB of prose. Measured on a live `gpt-5.5` run on 2026-10-07: the model narrated **1,857 bytes** of text before calling `optscale_kompressor_retrieve` with a valid handle, which arrived as **12,130 bytes of frames**; the relay gave up, the call reached codex, and the answer that came back was `"unsupported call: kompressor_retrieve"` — now permanent in that conversation's history, with the run reporting `retrievals 0` while the model had plainly asked. `Signal::Content` now carries the length of the *text* it saw and the grace counts that, so the number means what its name says. Every streaming test in the suite called the tool within a handful of frames, which is why nothing caught it; `a_turn_that_narrates_before_calling_is_still_intercepted` drives 120 deltas first, and asserts the fixture exceeds the grace in frame bytes while staying under it in text, so it fails if either the rule or the unit regresses.
- **Decode the request body before parsing it.** `Accept-Encoding: identity` handles the response direction; it says nothing about what the *client* sent. A client may compress its request body, and codex always does: `content-encoding: zstd` under a `content-type: application/json`, so the content type cannot distinguish an encoded body from a plain one. A proxy that parses the raw bytes there gets a body that will not parse, forwards it whole, and compresses nothing while every health check stays green. `bodycodec` reads `zstd`, `gzip` and `deflate`; an unknown coding, or a list of them, is left alone and takes the same fail-open path as before. Decoding is bounded by `proxy.max_body_bytes` on the **decoded** size, because a few kilobytes of zstd expands to as much memory as the sender likes. A body the engine did not rewrite is forwarded as the exact bytes that arrived; a body it did rewrite is re-encoded in the same coding, because the client's `content-encoding` is forwarded and has to stay true.
- Withhold the proxy's own `x-request-id` from the upstream request when [`authmode::classify`](../crates/kompressor-proxy/src/authmode.rs) reads the caller as `subscription` (a known agent User-Agent carrying something that is not a per-token API key). A subscription client is billed by request and recognised by the provider, so a header the real client never sends is drift the caller did not ask for. The id still reaches every stats row and log line. A `payg` or `oauth` caller keeps the header, because there is no subscription at stake and the correlation id is worth more.
- Add `X-Request-Id`, `X-Kompressor-Mode`, `X-Kompressor-Saved-Tokens` (estimate), `X-Kompressor-Retrieve-Rounds` to the response.
- Body size caps; `Content-Type` must be JSON for compression to be attempted; multipart/other ⇒ passthrough.
- `stream_options.include_usage = true` is **added** to OpenAI Chat streaming requests when absent (`proxy.force_usage = true`) so the stats can record billed tokens; the extra final chunk is stripped from the client stream if the client did not ask for it.

### 12.8 Failure policy

Any failure between steps 2–5 (parse, walk, compress, timeout) ⇒ forward the **original** body. Upstream errors are relayed as-is (status, body). The retrieve loop failing mid-way (store error, upstream error on a later round) ⇒ return the last complete upstream response to the client unchanged; never a synthesized answer. Per-request `tracing` span records where fail-open happened; a `kompressor_proxy_failopen_total{stage}` counter surfaces it.

### 12.9 SSE framing (`kompressor-proxy::sse`)

The relay holds frames back while it decides whether a turn is its own. That
hold is bounded by `stream_buffer_bytes` — an overflowing buffer relays rather
than growing.

A second bound on *time* exists (`stream_buffer_timeout`) and **ships off**. It
was added because the keepalive is an SSE comment and a client may ignore
comments when judging a stream idle; one agent gave up after five minutes having
been sent thirty of them, on three consecutive runs. Flushing early would let it
see real frames instead.

The first run with it enabled at 45s failed outright, and the reasoning behind it
was wrong. It looked like the same trade the byte bound makes. It is not: the
byte bound fires only after 256 KiB has been produced, by which point the turn is
plainly not a bare tool call, while the timer fires with *nothing* buffered —
exactly the window in which a reasoning model is about to emit one. It did:

The transcripts in this section quote the tool name as it stood when they were
captured. It is `optscale_kompressor_retrieve` now; the records are left as they
came back.

```text
error=unsupported call: kompressor_retrieve
Incomplete response returned, reason: max_output_tokens
Invalid 'input[14].arguments': string too long ... got 1552051
```

The flush handed the retrieve call to a client that never declared the tool; the
model ran to its output cap building a 1.5 MB argument; the next request was
rejected. 52 minutes, task failed. Nothing should enable the bound without first
solving how to keep a retrieve call from being flushed with the rest.

When a client does give up, the relay records what it was waiting on:
`state`, `held_ms`, `first_byte_ms`, `frames_held`, `bytes_held`. Those
distinguish "the provider sent nothing at all", which nothing here can fix, from
"the provider was talking and we were holding it back", which this bound does.

#### Bounding interception, and eliding our own call

Once the machine decides a turn is its own it holds *everything*, and until
2026-09-19 nothing bounded that. `stream_buffer_bytes` and
`stream_buffer_timeout` were both tested against `state == Buffering` only, so
they stopped applying at the exact moment the relay began holding the most. A
measured run paid for it twice in a row:

```text
state=Intercepting held_ms=300002 first_byte_ms=147 frames_held=9392 bytes_held=2551425
state=Intercepting held_ms=300002 first_byte_ms=144 frames_held=9764 bytes_held=2617546
```

Ten of that run's seventeen minutes, on one request retried once. `first_byte_ms`
of 147 says the provider was answering immediately; the relay was holding 2.5 MB
— ten times the buffering limit — and the client gave up at its own five-minute
mark, having been sent thirty keepalive comments it does not count as activity.

So `stream_intercept_bytes` (1 MiB) and `stream_intercept_timeout` (90 s) apply
in `Intercepting`. Both ship **on**, unlike `stream_buffer_timeout`, because
reaching them no longer flushes a retrieve call to a client that cannot answer
it — which was the whole reason that one is off:

**Our own call is elided rather than replayed.** The rule used to be that a turn
mixing our call with anything else is replayed byte for byte, on the grounds
that half a turn cannot be satisfied. That is right about the *client's* calls
and wrong about ours. A real run produced this:

```text
{"name":"kompressor_retrieve","arguments":"{\"ref\":\"k_9Z9xkNVTNAy3\",...}"}
{"type":"function_call_output","output":"unsupported call: kompressor_retrieve"}
```

— a wasted round trip and a model told the tool it had just been offered does
not exist. Now the frames of our call are dropped and the client's own call
reaches it untouched. Two frames describe the *turn* rather than one part of it
and are rewritten instead of dropped:

| Family | Frame | Rewrite |
|---|---|---|
| OpenAI Responses | `response.completed` / `response.incomplete` | our call removed from `response.output`, which otherwise repeats it in full |
| OpenAI Chat | the chunk carrying `finish_reason: "tool_calls"` | → `"stop"`, only when no other call remains |
| Anthropic | `message_delta` with `stop_reason: "tool_use"` | → `"end_turn"`, only when no other call remains |

The "only when no other call remains" condition is the difference between a
client that answers the call it owns and a client that waits forever for a
`tool_result` it was never asked to produce. Attribution of a frame to a call
comes from the same wire keys the turn accumulator already computes, so there is
no second parser that could disagree with the first.

A frame parser tolerant of `\n\n` / `\r\n\r\n` boundaries, `data:` multi-line joins, comment lines, and `[DONE]`; family-specific event decoders that only inspect the fields the state machine needs (`choices[].delta.tool_calls[].function.name`, `type: response.output_item.added`, `type: content_block_start`) and otherwise hand frames through untouched. Frames are never re-serialized in Passthrough.

## 13. Configuration

Layered, lowest to highest precedence: built-in defaults → config file (`--config kompressor.toml`, or `$KOMPRESSOR_CONFIG`, or `./kompressor.toml`, or `$XDG_CONFIG_HOME/kompressor/config.toml`) → environment (`KOMPRESSOR__SECTION__KEY`, double underscore; plus a flat `KOMPRESSOR_<FIELD>` alias for every engine knob) → CLI flags. `kompressor check-config` prints the effective merged config with secrets redacted and validates every range.

Two consequences of that search path are worth stating rather than leaving to be discovered. **`/etc` is not on it**, so the packaged system unit passes `--config /etc/kompressor/config.toml` explicitly, and a human running `kompressor stats` against that deployment must name the store too — with a user config present, a bare invocation silently answers from the wrong one rather than failing. **Paths are not expanded**: `$HOME` and `~` in a TOML value are literal, so `path = "$HOME/.local/share/kompressor/refs.db"` creates a directory called `$HOME`. The service templates substitute real absolute paths at install time for exactly that reason. QUICKSTART §9 has the commands.

```toml
# kompressor.toml — every key the configuration accepts, at its default.
# Verified against `kompressor check-config`; unknown keys are refused, so a key
# that is not here is a parse error rather than a setting that does nothing.

[server]
listen        = "127.0.0.1:8817"    # shared listener for selected surfaces
api_listen    = ""                  # per-surface listeners; empty means share `listen`
mcp_listen    = ""
proxy_listen  = ""
max_body_bytes            = 67108864   # bytes, not "64MiB"
max_inflight_compressions = 0          # 0 = CPU count x 2
queue_timeout   = "2s"              # then 503 with Retry-After
shutdown_grace  = "20s"             # statistics are drained within this

[auth]
mode   = "none"                     # none | api_keys
keys   = []                         # [[auth.keys]] key = "…", tenant = "…", mode = "…", enabled = true
secret = ""                         # X-Kompressor-Secret, for trusted internal callers

[engine]
mode = "recoverable"                # recoverable | aggressive | lossless
min_compress_chars = 512            # the size gate
max_array_items    = 10
max_field_chars    = 256
keep_tail_lines    = 20
ref_ttl_seconds    = 3600           # idle window, restarted on every read
ref_max_lifetime_multiplier = 8     # absolute cap = ttl x this
max_ref_bytes      = 1048576        # recoverable mode declines an oversized elision
enable_smart_crusher = true         # per-compressor switches
enable_search_compressor = true
enable_log_compressor = true
enable_diff_compressor = true
enable_tabular_compressor = true
enable_listing_compressor = true
enable_code_aware = true
enable_mixed_content = true
enable_config_compressor = true
enable_nested_routing = true
enable_redraw_fold = true           # arbitrated transforms
enable_build_templates = true       # build output may fold by name, §7.4
fold_handles_identical_repeats = false # a `[×N]` over identical lines reconstructs itself, §7.4
enable_html_compressor = true       # HTML folds its repeated lines, §7.10a
enable_normalised_text_fold = true  # a plain-text fold may normalise values, §7.10c
enable_tabular_ratio = true         # a table must be mostly rows, §6
enable_code_budget = true           # second pass when no body cleared the floor, §7.9
code_budget_min_body_lines = 6
enable_gutter_aware_detection = true # classify a numbered payload by its body, §6
enable_line_gutter = true
enable_fold_fallback = true
max_line_chars = 512                # last-resort clip, §7.13; 0 disables it
enable_lossy_key_factoring = true
enable_stale_read = true
enable_cross_turn_dedup = true
enable_preamble_aware_detection = false   # measured worse; see the field's doc comment
strict_lossless = true              # every compressor honours lossless
search_max_files = 8
diff_max_files = 8
code_max_body_lines = 24
min_nested_chars = 512
max_nested_depth = 2
max_code_bytes = 2097152
protected_roles = ["assistant", "system", "developer"]
token_guard = "chars"               # chars | tokens

[refs]
backend = "sqlite"                  # sqlite | memory
path    = "./data/refs.db"
recent_cache_entries = 4096         # write-through cache
purge_interval = "1m"
max_bytes = 1073741824              # bytes, not "1GiB"

[stats]
enabled = true
path    = "./data/stats.db"
queue_size = 16384                  # bounded; drops rather than blocks
flush_interval = "100ms"
retention_days = 90

[retrieve]
max_retrieve_chars = 32768

[mcp]
enabled = true
path    = "/mcp"
allowed_hosts = []                  # empty = accept any Host
max_retrievals_per_minute = 120

[proxy]
enabled = true                      # false = the proxy surface is NOT SERVED (404s)
compress = true                     # false = served, measured, forwards unchanged (§12.6)
min_tokens = 512                    # skip the compression hop below this
compress_timeout = "2s"             # exceeded ⇒ forward the original
max_body_bytes = 67108864
upstream_connect_timeout = "10s"
upstream_read_timeout = "10m"
retrieve_tool = "auto"              # auto | always | never
retrieve_execution = "server"       # server | client
retrieve_max_rounds = 3
retrieve_max_tokens_per_request = 16384
frozen_prefix = "marker"            # marker | first_seen, §12.10
keep_inline_max_percent = 40        # ceiling on the whole inline set, §12.6; 0 = none
stream_buffer_bytes = 262144
stream_buffer_timeout = "0s"     # off: flushing on a timer steals retrieve calls, §12.9
stream_intercept_bytes = 1048576 # bound on what one interception may hold, §12.9
stream_intercept_timeout = "90s" # and for how long; both on, because reaching them elides
stream_keepalive = "10s"
stream_content_grace_bytes = 8192  # bytes of TEXT held while waiting for a retrieve call, §12.9
force_usage = true                  # ask the provider for usage on streams
stale_read = true
websocket_relay = true              # on: serves both transports; a relayed turn reports no usage, §12.5b
prefix_cache_ttl = "0s"             # off: an age rule is a guess, §12.10
upstreams = []                      # see §12.1; empty means every path 404s
default_upstream = ""               # consulted only when no prefix matched
allow_insecure_upstream = false

[log]
level = "info"                      # RUST_LOG overrides
format = "json"                     # json | text
payloads = false                    # DEBUG: bodies and fragments (API), relay frames (proxy)
```

Durations are humantime strings (`2s`, `100ms`, `10m`); sizes are **plain byte
counts**, not `"64MiB"` strings.

Described elsewhere in this document and **absent from the schema**:
`auth.trusted_proxies`, `proxy.openai_frozen_prefix`, `proxy.shadow_sample_rate`,
`refs.redis_url`, `refs.key_prefix`, `refs.write_mode`, `refs.read_connections`,
`retrieve.default_range_lines`, `server.request_timeout` and `stats.rollup`.
Setting any of them is a parse error, not a silent no-op — which is the intended
behaviour, and the reason this block is generated from the schema rather than
written by hand.


Legacy env compatibility (Appendix D maps each): `KOMPRESSOR_MODE`, `KOMPRESSOR_MIN_COMPRESS_CHARS`, …, `KOMPRESSOR_REF_TTL_SECONDS`, `KOMPRESSOR_MAX_REF_BYTES`, `KOMPRESSOR_REDIS_URL` (accepted and **warned about**: there is no Redis backend in this build, and the warning says to set `refs.path` instead), `KOMPRESSOR_MIN_TOKENS` (→ `proxy.min_tokens`), `KOMPRESSOR_TIMEOUT` (→ `proxy.compress_timeout`), `KOMPRESSOR_ENABLED=false` (→ `engine.mode=off` at the proxy). `KOMPRESSOR_LOG_COMPRESSED` is accepted and mapped to `log.payloads` with a deprecation warning.

## 14. CLI

What the binary actually exposes today:

```
kompressor wrap [AGENT] [--port P] [--config PATH] [--mode M] [--attach]
                [--print] [--env NAME=root|v1] [--no-proxy] [--list] [-- ARGS…]
kompressor unwrap AGENT
kompressor serve        [--api] [--mcp] [--proxy] [--listen ADDR] [--config PATH]
kompressor mcp serve    [--config PATH]
kompressor compress     [FILE|-] [--mode M] [--role R] [--type T] [--config PATH] [--json] [--explain]
kompressor detect       [FILE|-] [--role R] [--config PATH] [--json]
kompressor retrieve     REF [--lines A:B | --pointer P | --grep RE] [--refs PATH] [--config PATH]
                        [--tenant T] [--json]
kompressor stats        summary|histogram|by-type|retrievals|request ID|store
                        [--from --to --tenant --format json|csv|table]
kompressor bench replay --corpus C [--config F] [--mode M]
                        [--save BASELINE | --compare BASELINE] [--check-determinism] [--json]
kompressor check-config [--config PATH]
kompressor version
```

Designed in this document but **not built**: `migrate` (schemas are
applied on open), `compress --ephemeral`, and the `bench sample`, `needles`
and `calibrate-tokens` subcommands (§18.6 describes the metrics). `bench
replay` and `bench extract` ship.

`compress --explain` prints the detection result, the chosen reading (mixed vs
whole, redraw arbitration), the per-slot kinds, markers and refs, and the
accounted-guard decision. It exists because every one of those is otherwise
invisible: "why did this payload not compress" is the question an operator
actually has, and without an answer the usual next step is to guess at a
threshold.

### 14.1 `wrap`: running an agent behind the proxy


#### Tools that cannot be wrapped, only instructed

Three of the four ways to point a tool at the proxy are automatable: set a
base-URL variable, edit a config file, or both. A GUI editor is the fourth, and
it is not automatable at all. Cursor is not a child process of `wrap`, so it
never sees the environment `wrap` exports; it has no config file this command
owns; and there is no CLI to launch. The only wiring that exists is a text box
in its settings.

`Wiring::Manual` is that case made explicit rather than faked. `kompressor wrap
cursor` starts or adopts the proxy exactly as any other wrap does, prints the
URL and the settings path, and holds the proxy up until Ctrl-C:

```
  Base URL:  http://127.0.0.1:8817/v1
  API key:   your own provider key, as usual

  In Cursor:
      Settings → Models → OpenAI API Key → Override OpenAI Base URL
```

`Endpoint::V1` because Cursor is an OpenAI-style client: it appends
`/chat/completions` to whatever it is given, so a root URL would produce a bare
`/messages` at the upstream. Its **Verify** button probes the endpoint before it
will accept the override, and that probe is a path the proxy does not compress —
measured: `/v1/models` and `/v1/chat/completions` both relay to the upstream and
return its `401` for a missing key, rather than the proxy's own `404`, so
verification behaves exactly as it would without the proxy in the path.

The precondition is printed **before** the URL, not after it. Tested against a
live editor on 2026-09-25 the other way round: the reader pastes the URL, sends
a prompt, and nothing arrives — the proxy recorded zero requests while probes
sent to it in the same minute were logged normally. The screenshot explained it:
the API-key field was empty and the override toggle was off, so the override was
never consulted. An instruction whose precondition arrives too late to act on is
an instruction that wastes the reader's time.

**The subscription path cannot be compressed, and that is most of Cursor's
users.** On a Cursor plan every request goes to Cursor's own servers over the
same protobuf Connect API `cursor-agent` uses (below) — this proxy never sees
it, and could not read it if it did. The override exists only for a
bring-your-own OpenAI key.

**This path is documented but unverified end to end.** What has been measured is
that the endpoint is reachable — `/v1/models` and `/v1/chat/completions` relay
to the upstream and return its `401` rather than the proxy's `404`, so Cursor's
**Verify** button has something to talk to. That the editor then sends
OpenAI-shaped chat completions through it is taken from Cursor's documented
bring-your-own-key behaviour, not from traffic anyone has watched arrive here.
No Cursor request has ever reached this proxy.

One more limit is printed with the instructions:
**Cursor cannot send custom headers**, so a session is attributed to no
project and no tenant beyond the anonymous default. headroom works around this
  by encoding the project as a `/p/<name>` prefix on the base URL and stripping
  it in the proxy; OptScale Kompressor does not, so savings land in one bucket.

**The Cursor Agent CLI is a different product and cannot be compressed.**
`cursor-agent` looks like the obvious thing to wrap — it is a CLI, it takes
`--endpoint`, and it reads `CURSOR_API_ENDPOINT`. Pointed at a recording server,
version 2026.09.23-86fc751 turns out to speak **Connect RPC carrying protobuf**:

```
POST /aiserver.v1.ServerConfigService/GetServerConfig
  content-type: application/proto
  connect-protocol-version: 1
  user-agent: connect-es/1.6.1
```

There is no JSON message array for a wire walker to find and no OpenAI or
Anthropic shape to route, so the engine has nothing to read. Pointed at the
proxy for real, every path 404s and the agent stops with `Error: [unimplemented]`.

The catalogue carries it as `Wiring::Unsupported` rather than leaving it out.
Omitting it is worse: `wrap` would answer "unknown agent" and recommend
`--env NAME=root|v1`, which for this tool sets a variable that either does
nothing or routes it into a proxy that cannot answer — the silent failure this
catalogue exists to prevent, reached by the helpful road. The entry is refused
before a proxy is started or a config file is touched, and the refusal says why
and points at `kompressor wrap cursor`, which is the path that does work.

The zero-traffic warning that closes every other wrap (§14.1) is suppressed for
manual wiring. That message reads "the agent used a different base URL than the
one set above (most likely: it ignores that variable)", and for a tool that was
never launched and whose environment was never set, every clause of it is false.
A manual session that routes nothing means the user had not finished pasting.

**A wrapped agent that pins its whole prompt compresses nothing.** Claude Code
puts a `cache_control` breakpoint on its final message, so every wrapped session
records `outcome = prefix_frozen` and removes zero bytes (§9.4). The wrap still
works — traffic routes, the answer comes back, the proxy is transparent — it
simply has nothing it is allowed to do. `codex` does not use explicit
breakpoints, so it compresses normally. Check `v_outcomes` before concluding a
wrap is broken.
Everything the proxy does is available to anyone willing to start it, point a
base URL at it, mount an upstream for their provider, and stop it afterwards.
Each of those steps is small, and every one of them fails the same way: the
agent talks to the provider directly, nothing is compressed, and nothing says
so. `wrap` is the single step that does all of them.

```
wrap claude ── resolve port ──▶ serving? ──yes──▶ adopt (never stopped by us)
                                    │no
                                    ▼
                      write config (user's + upstreams)
                                    │
                      spawn `serve --proxy --api`, own process group
                                    │
                           poll /healthz until ready
                                    │
       ┌────────────────────────────┴───────────────────────────┐
       ▼                                                         ▼
  launch: env + run agent in foreground              attach: write settings file,
       │                                                     wait for Ctrl-C
       ▼                                                         ▼
  agent exits ──────────▶ unregister; stop proxy only if we started it
                           and no other live session still holds it
```

**Pointing the agent at the proxy.** Every agent already reads a base-URL
environment variable; wrapping sets it. The two URL shapes are not
interchangeable — an Anthropic-style client is given a host and appends
`/v1/messages` itself, an OpenAI-style client is given a URL that already ends
in `/v1` — so the catalogue records which shape each tool wants. An entry is a
claim that a particular variable works for a particular tool, and a wrong claim
fails silently, so the catalogue is short and `--env NAME=root|v1` exists to
wrap anything not in it without guessing on the user's behalf.

**Mounting an upstream.** `proxy.upstreams` is empty by default and an unmatched
path is a 404, so a wrap that added nothing would start a proxy that refuses
every request the agent makes. The agent's providers are layered onto the user's
own configuration; an upstream whose name the user already defined is left
alone, because an explicit mount is a decision. The merged set is sorted
longest-prefix-first for stable configuration output, although routing itself
now chooses the longest matching prefix regardless of who wrote
which entry.

**Sharing one proxy.** Two wrap sessions in two terminals should share a proxy,
and neither knows the other exists. Each drops a marker file naming itself under
`$KOMPRESSOR_HOME/run/clients-<port>/`; the last session to remove its marker
stops the proxy. A marker is believed only while its owner is alive, which is
answered with the process's start time rather than a bare pid — pids are reused,
and a marker from a session killed yesterday can name something unrelated today.
A proxy this session did **not** start is never stopped by it.

**Editor-hosted agents.** Environment variables only reach a process this command
launches. An agent started by an editor inherits the editor's environment, so
`--attach` writes the base URL into the settings file the agent reads for itself
and holds the proxy up until interrupted. That turns a one-line change into
shared mutable state on disk, with three failure modes and one answer to all of
them — a sidecar recording who owns each key and what it held before any of them
arrived:

| Problem | Without the record |
|---|---|
| Two sessions, one file | A read-modify-write from each loses one of the updates. |
| The second session sees the first one's URL | Whoever exits last restores a *dead proxy's* URL as if it were the original. |
| A session is killed and restores nothing | Its claim pins the file forever. |

The first live claimant records the original; later ones inherit it. A claim is
dropped when its owner dies. `kompressor unwrap AGENT` forces the restore
regardless of who holds it, which is what recovers a project whose wrap session
was killed.

#### 14.1a Offering the retrieval tools without leaving residue

A wrap points an agent at the proxy. It can also hand the agent the tool server
of §11.2, and for Claude Code it does — `Agent::mcp` carries
`McpWiring::Flag { flag: "--mcp-config" }`, measured against 2.1.278.

The reason to bother is that proxy-side retrieval has a hard boundary. The
engine injects `optscale_kompressor_retrieve` into the request and the proxy answers the
call inside the same round trip, so the client never declares the tool and never
needs to — but that loop only exists while a request is in flight. A handle the
model meets anywhere else is dead text: in a file read later, in a transcript
from a previous session, in output a sub-agent captured. A tool server is held
open for the whole session, so a handle is resolvable whenever one is met.

The question is where the registration lives, and the two answers are not
equivalent:

| | Durable registration | Command-line offer |
|---|---|---|
| Where | the agent's own MCP config | a file in `$KOMPRESSOR_HOME/run/` |
| Survives a `SIGKILL` as | an entry naming a binary that may have moved | two inert files nothing reads |
| Needs an `unwrap` | yes | no — there is nothing to undo |
| Reaches `--attach` and GUI agents | yes | no |

The command-line offer is chosen, and the row it loses on is real: `--attach`
and `Wiring::Manual` exist precisely because the agent is started by something
other than `wrap`, so there is no command line to put the flag on, and those
sessions go without. That is the narrower behaviour, taken deliberately.
Residue that outlives the thing explaining it is the worse failure — and unlike
the settings-file case above, it buys nothing here, because a claim-and-restore
sidecar cannot help with a registration whose whole point is to be durable.

Two correctness details:

- **The store is pinned.** `refs.path` defaults to the relative
  `./data/refs.db`, and the tool server is spawned *by the agent*, from whatever
  directory the agent is in. Left relative it would open a different SQLite
  file, create it without complaint, serve `tools/list` normally, and then miss
  every handle — reported to the model as "the fragment expired", which is a
  legitimate answer and so would never look like a fault. `wrap` resolves the
  path to an absolute one before writing the session configuration.
- **No credential is copied.** The registration's `env` is empty. The tool
  server inherits the agent's environment, which is the one `wrap` is already
  running in, so a deployment that set `KOMPRESSOR_API_KEY` already has it —
  and naming it in the document would write a secret to disk for no gain.

## 15. Observability

- **Tracing**: one span per request — `request` on the API surface and `proxy`
  on the proxy, both carrying `request_id`. INFO lines for the events worth
  seeing (a compression summary, an upstream stream ending early, a client
  hanging up, a compressor panicking). Payloads only under `log.payloads`.

  The subscriber is installed by `serve` from the `[log]` section, with
  `RUST_LOG` taking precedence. That is worth stating because it was missing
  until a smoke run produced a zero-byte log file for an incident: `tracing`
  calls are inert unless something consumes them, so every `info!` and `warn!`
  in every crate went nowhere while `log.level` and `log.format` configured a
  subscriber that was never built.

  **Not built**: the per-stage child spans this section once promised
  (`detect`, `compress`, `store.put`, `upstream`, `retrieve_round`). Timing is
  available instead as `engine_ms` on each `optimization_events` row and
  `upstream_ms` / `total_ms` on each `proxy_requests` row.
- **Prometheus** (`/metrics`): `kompressor_compress_requests_total{surface,mode,outcome}`,
  `kompressor_tokens_in_total`, `kompressor_tokens_out_total`,
  `kompressor_saved_tokens_total{mode}`, `kompressor_compress_duration_seconds`
  (histogram), `kompressor_messages_by_kind_total{kind}`,
  `kompressor_protected_total{reason}`, `kompressor_refs_emitted_total`,
  `kompressor_ref_bytes_emitted_total`, `kompressor_refs_live`,
  `kompressor_refs_bytes_live`,
  `kompressor_retrievals_total{outcome=hit|miss|expired,ranged}`,
  `kompressor_retrieved_tokens_total`, `kompressor_engine_saturated_total`.

  **Not built**: `kompressor_engine_ms_per_kb`, `kompressor_engine_panics_total`,
  `kompressor_proxy_requests_total`, `kompressor_proxy_retrieve_rounds`,
  `kompressor_proxy_upstream_duration_seconds`, `kompressor_proxy_failopen_total`
  and `kompressor_refs_dropped_total`. The proxy ones matter most: a deployment
  scraping metrics cannot currently see proxy status codes, fail-open stages or
  retrieve rounds without reading `stats.db`.
- **Health**: `/healthz` liveness (always 200 while the process runs); `/readyz`
  checks the stats writer thread and ref store are alive and, for the proxy,
  that every configured upstream resolved DNS at least once.

## 16. Security and multi-tenancy

- Refs are keyed by `(tenant, ref)`; there is no API that lists or reads across tenants. Handles are 72-bit content hashes: unguessable in practice, but a leaked handle alone is useless without the tenant's key.
- API keys are compared in constant time; never logged; never stored in stats (tenant only).
- The proxy forwards client credentials to the configured upstream only; it never sends them to any other host. Upstream `base_url` must be absolute `https://` unless `allow_insecure_upstream = true` (for local Ollama).
- Body limits, header limits (tower-http), per-tenant retrieval rate limits, retrieve round caps. No shell-outs, no file reads outside the configured DB paths, no network in the engine.
- Fragments may contain secrets (they are tool output). They live in `refs.db` with the TTL; `refs.path` should sit on encrypted or ephemeral storage in shared environments; `--ephemeral` keeps them in memory. Deleting one tenant's refs and stats needs a `kompressor purge --tenant T`; **it is not built**, so today that is a SQL `DELETE` against `refs.db` and `stats.db` while the service is stopped.
- Content from providers and from stored fragments is data: nothing in the proxy interprets instructions inside payloads (the retrieve loop acts only on structured tool calls with our exact tool name).
- The binary is `forbid(unsafe_code)` except the isolated tree-sitter FFI; grammar parsing runs on bounded input (1 MB detection sample; code payloads are capped at `engine.max_code_bytes`, default 2 MiB, larger files decline).

## 17. Performance and concurrency model

Targets (per 1 KB of eligible payload, single core, release build): detection + compression **< 0.1 ms/KB** median (measured: 0.14–0.17, see §19; regex-heavy detection over a 1 MB sample is the dominant cost), p99 under 5 ms for 100 KB tool outputs, arbitration overhead < 20% on mixed bodies. Memory: one `Payload` (content + cached line index) plus at most two scratch outputs per arbitration.

Design choices that get there:

- One `Payload` struct per message computes `chars_len`, the line index (`Vec<Range<usize>>` from `split_lines`), the 1 MB sample slice and `has_cr`/`has_ref_literal` flags once; every predicate takes `&Payload`.
- `RegexSet` for membership counts; `memchr`/`memmem` for literal gates (`'ref:'`, `'\r'`, `'```'`, `'diff --git'`).
- No per-line `String` allocations in folding: templates written into a reused buffer; the fold index borrows.
- JSON parsed once (detector → crusher handoff).
- Tree-sitter grammars are statics; parse trees dropped immediately after replacement.
- Engine calls run on `spawn_blocking` (default) or a dedicated `rayon` pool sized `cpus` (`server.engine_pool = "blocking" | "rayon"`); the tokio runtime stays responsive for streaming relays.
- Store writes and stats are off the request path; reads hit the recent cache first.
- Proxy relays stream frames zero-copy (`Bytes`) in Passthrough; only the Buffering window copies.

Benchmarks (`criterion`) per compressor on fixture payloads and end-to-end on the committed corpus; `kompressor bench replay` prints ms/KB and the latency tail, with grammar warm-up before timing.

## 18. Testing and benchmarking

### 18.1 The invariants, checked rather than asserted

Seven properties are checked on every corpus payload in every mode, and none is
tradeable against compression ratio:

| Invariant | Meaning |
|---|---|
| `longer_than_input` | never: a compressor that fails to shrink yields the original |
| `mutated_while_declining` | never: a declined payload comes back byte-identical |
| `orphaned_ref` | never: every stored handle is referenced by the output |
| `refs_without_marker` | never: a handle always arrives with something announcing it |
| `nondeterministic` | never: re-compression is byte-identical |
| `empty_ref_fragment` | never: a handle always resolves to content |
| `fold_count_unsupported` | never: a handle-free `[×N]` claims a repeat the input holds |

The last one is newer than the rest and exists because the bare fold is the only
marker that asserts what the removed bytes *were* rather than merely that some
went — which is what lets it carry no handle. It is checked where the mode
promised a way back, so with the collector on; aggressive keeps no fragments at
all and its bare folds count template matches, which 126 corpus payloads
exercise. The count is compared against the input read three ways, because
`lift_gutter` strips a `grep -n` column before anything folds and re-attaches it
only to the lines that survive (§6).

Alongside them: every shrink is accounted for (an omission marker, a declared
reversible envelope, or a provably layout-only rewrite); protected roles, frozen
prefixes and flagged messages come back untouched with their handles reported;
`lossless` drops nothing; in `recoverable` every elision is either a fetchable
handle or a marker that does not claim to be reversible — a fold whose fragment
is over `max_ref_bytes` is **refused**, not degraded to a bare count, and every
occurrence stays where it stood (§7.4); and output must not depend on the
surrounding conversation (§18.4).

### 18.1a How determinism works

![determinism](charts/determinism.drawio.png)

Determinism is the invariant that pays for the others, and it is worth stating
why before stating how. Providers bill a cached prompt prefix at roughly a tenth
of a fresh one, and an agent re-sends its whole history every turn — so if a
payload compresses to different bytes on turn six than it did on turn five, the
cached prefix stops matching and the entire conversation is re-priced at full
rate from that point on. On the codex session measured on 2026-09-22 that would
have turned $0.44 into $1.64: more than compression saved. Instability does not
cost a little accuracy, it costs more than the feature is worth, which is why
`nondeterministic` sits beside "never larger" as an invariant rather than a
preference.

The guarantee is that `compress_content(content, role, options)` is a pure
function: the same inputs yield byte-identical output in any process, on any
machine, whatever the ref store already holds. `kompressor-engine` has no I/O and
no async at all — it is free functions over a payload, with no `SystemTime`, no
`Instant` and no `rand` anywhere in the crate — which is what makes the claim
checkable rather than aspirational, and what lets the whole engine be replayed
against a corpus.

Four rules keep it true. **Ordered containers wherever order is observable**:
Rust seeds every `HashMap` per process, so iterating one would reorder output
between runs of the same binary, and `BTreeMap` is used everywhere a traversal
reaches the output (`by_type`, `protected_by_reason`, the event fields). Hash
containers survive only for membership and counting, never iterated into bytes —
the log fold's template index groups lines but emits them from a `Vec` in input
order, `tabular`'s keep-set is only ever asked `contains`, and the detector's
offset maps are reduced with `max` and `count`. **Content-addressed handles**:
`k_` + base64url of `blake2b(fragment, digest_size=9)`, twelve characters
derived from the bytes alone, so nothing counts, nothing is sequential and
nothing consults the store — the same fragment yields the same handle on another
machine, in a later run, after the store has been wiped, which is exactly why a
marker stays byte-identical across the turns that re-send it. **No compressor
may read the conversation around the payload**: ranking a tool result against
the question that prompted it is the obvious way to compress harder and is
forbidden for this reason, since the same bytes would then compress differently
on every turn they survive into. **Arbitration decides by length**: where two
readings compete — the gutter lift, the redraw fold, the diagnostic echo — both
are computed and the shorter ships, a total order over the same two candidates
every time, with the loser's fragments rolled back and the winner's re-stashed
under the very handles its markers already carry.

There is one documented exception. A compressibility heuristic asks zlib how
well a payload deflates, and the exact byte count a deflate backend returns is
its own business and may differ between library versions. That number is
therefore only ever allowed to move a *budget* and must never reach marker
bytes: determinism is exact within a build, which is the property prompt caching
actually needs.

It is checked four ways, three of them before anything ships. The recorded
vectors (§18.2) pin exact bytes for every corpus payload in every mode, 822
comparisons. `bench --check-determinism` re-compresses every payload and
compares, reporting `nondeterministic`. The same pass re-compresses with the
preceding user turn blanked — blanked rather than perturbed, because a
compressor that reads the question has to behave differently for *some*
question and the empty one cannot coincide with the real one — reporting
`query_dependent`. And in production the ratio `cache_read_tokens /
prompt_tokens` is the live signal: 81.0% on the codex session above, 92.7% on an
earlier terminal-bench run, neither reachable if the prefix were being
rewritten, and a drop in it is the first symptom of a compressor that moved.

### 18.2 Recorded vectors

Compression behaviour is a pile of interacting thresholds, so the regression that
matters is not a crash — it is a change in *which payloads compress and to what*,
arriving as a side effect of tuning something else. Unit tests do not catch that;
a recorded snapshot does.

`fixtures/golden.json.gz` holds, per payload and per mode, the detected type and
the metadata that steered it, the compressed bytes, the reported kind with its
unfolded applied types, and the handles emitted in order. Handles are
content-addressed, so equal handles prove equal fragments without the vectors
storing a byte of them. Two corpora are covered: 240 payloads of real agent tool
output, and 34 hand-written micro payloads that reach detector branches the real
corpus does not hold — 822 comparisons in total.

Re-recording needs the generator's own parser. `engine/code.py` imports
`tree_sitter_language_pack` inside a `try`/`except ImportError` and silently
skips the code compressor without it, so an export from an environment that
lacks it records six `source_code` payloads as untouched — a regression written
*into* the file that exists to catch regressions. The check after any re-record
is therefore that the existing records are unchanged and the diff is purely
additive.

`cargo test -p kompressor-engine --test golden` replays all of it. Several engine
behaviours have deliberately changed since the vectors were captured, and each
lives behind a flag; the comparison sets those flags to the recorded state and
the changed behaviours get their own tests. A payload that differs for any other
reason is a regression. Two payloads differ deliberately, each recorded in a
table the test enforces — and a recorded difference that *stops* happening also
fails, so the table cannot quietly grow stale.

`tools/export_golden.py` regenerates the vectors. That is a deliberate act: it
moves the snapshot the test compares against.

### 18.3 Property and fuzz tests

- `proptest`: arbitrary JSON values (the crusher never panics, output parses,
  never larger, lossless round-trips through the envelope decoder); arbitrary
  line soups with carriage returns (redraw overlay idempotent, CRLF preserved);
  arbitrary text (the detector is total and deterministic; `split_lines` agrees
  with a generated table of Unicode separators); the marker scanner (`refs_in`
  finds every embedded handle and no non-handle).
- `cargo-fuzz` targets: `detect_content`, `crush_json`, `crush_mixed`, the SSE
  frame parser, and the wire walkers — where the round trip is the property:
  walk, write back with no changes, and the JSON tree must be byte-identical.

### 18.4 Query stability

Scoring a payload against the surrounding conversation looks like an improvement
and is a cache-buster: an agent re-sends its history every turn under a different
latest question, so query-dependent bytes change a prefix the provider is
caching. The trade was measured and lost, so the one query-aware ranking the
engine had was removed. `no_payload_is_query_dependent` compresses every corpus
payload under three fixed probe questions and requires byte-identical output.

### 18.5 Integration tests

- **API**: axum `Router` tests over memory stores — auth, compress, raw compress,
  retrieval events, protected roles, handles inside protected content, block
  content, ranged retrieval, and the stats endpoints against a temporary
  `stats.db`.
- **Store**: SQLite on a temp file — idle TTL restart on read, lifetime cap, lazy
  purge, size-cap eviction, WAL cross-process read from a second connection,
  migration from the v1 schema; the Redis backend behind a feature-gated job.
- **MCP**: an `rmcp` client against the streamable-HTTP server, and the stdio
  transport driven over an in-memory pipe (`serve_pipe`) so the path is covered
  without spawning a process — `tools/list`, each tool, tenant from header,
  rate limit.
- **Proxy**: `wiremock` upstreams per family. Passthrough below threshold;
  compression applied only to compressible slots with the rest byte-identical;
  frozen floor respected; `previous_response_id` untouched; tool injection only
  where markers exist; the server-side retrieve loop both streaming and not, for
  each family; mixed tool calls handed to the client; round cap; miss and expired
  results; usage summation with intermediate rounds hidden; fail-open on compress
  timeout, malformed JSON and upstream 5xx mid-loop; header policy; and
  `include_usage` injection and stripping.
- **Cross-surface**: handles minted by the proxy are retrievable through the API
  and MCP within the same tenant, and invisible to another.

### 18.5a The bug hunt: the code compressor and the retrieve loop together

Unit tests prove the loop turns. They cannot prove it turns on the content the
model actually needs, because the model is not in them. The bug hunt puts it
there, against a live client and a live subscription, and asks the one question
the whole retrieve design exists to answer: *when a body has been elided, can
the model get it back and use it?*

Eight source files — one per language in the detector's code table — each with
twelve near-identical functions, one of which reads one element past the end of
its array. Every body clears `code_max_body_lines`, so the compressed view is
twelve indistinguishable markers (88–91% saved, twelve handles per file).

Two properties make the answer mean something.

**Determinism.** Re-reading a file gives the same elided view, so there is no
second path to a body. This is the same property §18.1a exists to protect,
used here as an instrument.

**No distinctive line.** Every function ends with the byte-identical
`return rows[bound]`, and `bound` is assigned mid-body, inside the elided
region, as `last` eleven times and `size` once. No known-bad shape
(`rows[len(rows)]`, `rows[rows.length]`) exists anywhere in the tree. An
earlier draft used those shapes and the model solved it with one `grep`,
retrieving nothing: search output is a few short lines, short output is not
worth compressing, and the faulty line arrived verbatim. **Compression bites on
large reads; a targeted search routes around it** — a real property of the
deployment, not a fixture wart, and the reason the prompt forbids content
search and the harness checks the transcript rather than trusting it.

Three things are measured, and a correct answer alone is not one of them.
Twelve identical markers give a one-in-twelve guess, so the verdict requires a
fragment *containing the faulty line* to have come back — a join from
`retrieval_events` to the store, since `retrievals > 0` says only that the loop
turned, not that it turned on the right body. The function name appears in
twelve signatures in every file, so "did it answer `reconcile07`" is read from
the final answer alone; grepping the transcript matches the fixtures.

Measured 2026-09-24, eight languages, `retrieve_max_rounds = 16`: eleven
messages compressed for ~13,086 tokens saved (12,121 net of retrieval), twelve
retrievals, twelve hits, no misses, **eleven of the twelve ranged** — the model
sliced ~50 tokens out of a body rather than pulling the whole thing back — one
full body fetched, and the faulty function named. The loop closes.

One limit the verdict does not hide: the check joins `retrieval_events` to the
store and asks whether the *stored fragment* holds the faulty line. It cannot
ask whether the *returned slice* did, because the returned text is not
recorded. With ranged retrieval that is a real gap, narrowed only by the fact
that a run also fetched one body whole.

The shipped budget of three rounds cannot complete this task. A model that
fetches one body per turn is refused after three of twelve, measured twice,
and asking it to batch its calls into one turn did not change that. The stage
therefore runs against a raised budget and prints the value it used.

The harness is `kompressor-codex-sweep/run.sh bughunt`. It found the refusal-relay
defect in §12.6, and the Rust row of the detector's code table (§7.9): the
pattern required a bare `fn `/`struct `, so a library file declaring everything
`pub` matched nothing and was claimed by the listing rule — OptScale Kompressor's own
source among them.

Every stage that spends money or calls `reset` takes an exclusive lock. `reset`
deletes the stats and refs stores and restarts the proxy, so a second run
started while one is in flight removes the rows the first is about to read.
That happened twice in one morning and both times the damage read as a defect
in the thing being measured.

### 18.6 Corpus benchmarking


**Image blocks are not payloads.** An agent that takes a screenshot sends it as
an `input_image` block carrying a base64 data URI, and the wire walkers offer
only text to the engine — so those bytes are never compressed and must never be
counted. They are easy to count by accident: extracting "tool output" from a
Harbor rollout picks them up, and 428 of them made a 51 MB corpus measure as
176 MB and its coverage read 88% instead of 59%. Anything built from a rollout
has to drop blocks whose head contains `input_image` or `base64` first.

Once they are dropped, the real distribution has no tail: the median tool output
is 2.3 KB and the largest is 40 KB, because the harness truncates there.
`kompressor bench replay` scores a corpus on three numbers:

- **coverage** — the share of eligible bytes removed;
- **claimed** — the share some compressor took, whether or not it shrank much;
- **billed** — coverage weighted by re-sends at `1 + (requests − 1) · 0.1`, since
  a payload re-sent 261 times costs roughly 27 first-sends once the provider's
  prompt cache is charging the cache-read rate.

Results are bucketed by `<compressor>` or `untouched:<detected type>`, reported
with ms/KB and the latency tail, and checked against the invariants above.
`--check-determinism` re-compresses everything to prove the bytes are stable.
`--save` records a baseline and `--compare` scores against one; per-bucket
regressions are compared on **stable membership only**, because a grammar that
moves files from `untouched:source_code` into `source_code` lowers that bucket's
average while improving the corpus. `fixtures/baseline.json` ships a recorded
floor, and a ratchet test fails when coverage improves beyond tolerance without
the baseline being re-recorded.

Grammars are warmed before timing: the first C payload costs 2.5 s cold against
62 ms warm, which would otherwise dominate any measurement.

### 18.7 CI

`.github/workflows/ci.yml`, five jobs, each one running a Makefile target so
that `make check` locally and CI cannot hold different opinions:

| job | what it runs |
|---|---|
| `lint` | `make lint` — `cargo fmt --check`, `cargo clippy --all-targets -D warnings` |
| `test` | `cargo test --workspace`, twice: default features and `--no-default-features` |
| `msrv` | `cargo check --workspace --all-targets` on 1.90 exactly, so the declared MSRV is a fact rather than a claim |
| `gate` | `make release` then `make bench` (the fixture replay gate: coverage, claimed and billed not regressed, no invariant violated, determinism), `make docs-check`, `make smoke` |
| `e2e` | `make e2e` — the hermetic Docker suite on an `internal: true` network |

There is no `--features redis` matrix, because there is no such feature.
`cargo deny`, the musl/aarch64 static builds, the `FROM scratch` image and the
`.deb` are described in §20 and are **not** wired into CI; `make deb` and the
Dockerfile exist and are run by hand.

## 19. Status

Built and passing: ten crates, **1,157 tests**, clippy and rustfmt clean. Measured
numbers and the current gap list are in the README; what is *not* built is
recorded there too rather than left to be discovered.

`kompressor wrap` (§14.1) is built, including the shared-proxy reference
counting, the settings-file ownership record, and the session-scoped tool-server
offer of §14.1a. The stdio MCP transport it depends on (`kompressor mcp serve`,
§11.2) is built as well. Verified end to end against a
mock upstream: a 31,969-character tool result reached the upstream as 1,826
characters with the user turn byte-identical, two concurrent sessions shared one
proxy and the second to exit stopped it, and a settings file was restored to its
pre-wrap value — including after its owning session was killed.

The engine holds all seven invariants on both corpora in all three modes, with
determinism verified. `recoverable` is the default on every surface, and every
recoverable marker retrieves its exact fragment through the API, through MCP and
through the proxy loop — streaming and buffered alike.

Known incomplete, in the order it would be worth finishing:

| | |
|---|---|
| Needle retention | the measure of whether compression kept what the agent went on to *use*. It is the one quality measure that can fail while every ratio improves, which is exactly why its absence matters. |
| `bench sample` / `needles` / `calibrate-tokens` | `replay` and `extract` are built; these three are not, so token weights and a stratified sample still cannot be regenerated by this binary. |
| `migrate` | §14 once listed a migration command; it does not exist. Schemas are applied when the store is opened. |
| Redis fragment backend | designed (§8.3), not built. Multi-instance deployments sharing one fragment store need it. |
| `proxy.shadow_sample_rate`, `proxy.openai_frozen_prefix` | specified in §12.2–§12.3, absent from the configuration. Both were optional and both default to off. |
| `X-Kompressor-Retrieve-Rounds` on streamed responses | always 0, because response headers necessarily precede the loop. The real count reaches `proxy_requests`. |
| ~~`PrefixCache::Cold` reaching a pinned slot~~ | **Closed.** `cache_control` turned out to protect in four places, not two (§5.12b): `effective_frozen`, the message `flagged` reason, `TextSlot::protected`, and `SlotOrigin::protected` at write-back. `Protection { Always, WhileCached }` carries the reason across the wire-to-engine boundary and all four lifts are load-bearing — each was verified by breaking it and watching `a_cold_prefix_is_compressed_even_where_cache_control_pinned_it` fail. |
| ~~**Cache churn on the first-handle turn under `first_seen`**~~ | **Closed — this was the row below, seen from the other side.** It was recorded as open with *two hypotheses falsified*, and one of those two (retrieve-tool injection perturbing `tools`) turned out to be the answer: the test that "falsified" it minted no handle and so demonstrated nothing. Kept struck through rather than deleted because the wrong conclusion is the instructive part — a broken test is not a refuted hypothesis. |
| **A retrieve round sometimes re-writes the whole prefix on Anthropic** | Open, and weaker than first recorded. On one run the turn with `retrieve_rounds = 1` read 28,498 and wrote 28,498 — a complete re-warm, $0.178 or 11.9% of that run to put back 459 tokens, taking 46.6 s. **Two later runs contradict the generalisation**: their retrieve turns wrote 598, 796 and 3,463, so the second round hit the cache. Note that `read` and `write` are summed across rounds on such a turn (`add_usage` is correct to do so), which makes the incremental-cache identity inapplicable there and produces a `+N` then `−N` pair that is not churn. Why one run's second round missed and others hit is unexplained. |
| ~~**The first handle invalidates the whole prefix**~~ | **Root-caused and fixed.** `retrieve_tool = "auto"` injects `optscale_kompressor_retrieve` the turn a handle first appears, and `tools` precedes `messages`, so the array moved ahead of every cached message: measured, `cache_read` went 16,101 → **0** in one turn, accounting for all 19,705 excess writes of that run. Proven rather than argued — `the_turn_that_mints_the_first_handle_leaves_the_tools_block_alone` holds it fixed: the tools array is offered from the first request of a recoverable conversation, so the turn that mints the first handle no longer moves it. A first attempt to test this minted no handle at all and was deleted for demonstrating nothing; the fixture now asserts a handle was minted before it concludes anything. |
| ~~**`RetrieveTool::Always` did not mean always**~~ | **Fixed.** Injection ran only at the end of `prepare_inner`, past seven `return Prepared::passthrough` sites, so a turn that declined or whose rewrite was discarded forwarded the client's original bytes with no tool in them. Measured with `always` set: the tool first appeared on **turn seven**, because turn three was `no_compressible_content` and turns four to six were discards — and the array flipped exactly where the setting existed to hold it still. The four post-walk decline paths now go through `passthrough_with_stable_tools`; `a_turn_that_compresses_nothing_still_carries_the_same_tools` fails without it. |
| **`first_seen` economics unmeasured** | Safety is now well established — **excess writes 0 on 10 of 13 runs**, every non-zero figure predating the `was_warm` and discard-guard fixes, and a 40-turn Opus run holding the incremental-cache identity 37/37 while removing 33.0% at 96.6% cache reads. What is still missing is the COST case, and the obvious experiment is invalid: `$/MTok out` correlates **+0.80 with run length**, so two runs of different trajectories cannot be compared however carefully they are matched. Sample sizes from 21 runs: a 10% effect needs ~34 runs per arm, 5% needs ~136. This wants a replay-based A/B over the same recorded turns, not more live runs. Until then the default stays `marker` — on economics, no longer on churn. |
| ~~**Conversations could not be told apart**~~ | **Closed.** The key was a fixed selection of opening messages, which an agent harness makes identical across tasks: 20 tasks collapsed into five keys that repeated across runs. Now a learned opening prefix (§12.10). Measured after: one identity per trajectory, accuracy unchanged task-for-task. The same change removed a second derivation of the key inside `prepare_inner` that used a *different* rule — they agreed only by coincidence, and three `first_seen` tests went to compressing nothing the day they diverged. |
| ~~**The first turn of a conversation read as `Cold`**~~ | **Closed.** A bare `cache_read == 0` is turn one, not a cache miss, so the prefix was thawed on turn one and re-frozen on turn two — a rewrite. `Seen::was_warm` is sticky, so a lapse is warm-then-zero and never a bare zero. Cost 8,299 excess writes on a measured run. |
| ~~**Discarding a rewrite broke prefix-monotonicity**~~ | **Closed.** Throwing a rewrite away forwards the client's original bytes, which under `first_seen` differ from what the prefix already holds. Both discard guards now gate on `discard_would_break_the_prefix`. |
| ~~**The corpus could not be rebuilt**~~ | **Closed.** `kompressor bench extract` reads what real runs elided out of their ref stores. Replaying a corpus built that way immediately found a false positive in the safety checker: `check_bare_folds` split only on `\n`, so dpkg's `\r`-separated redraw frames turned six honest folds into "violations". |
| **Stale-read is not prefix-monotonic** | Gated rather than fixed (§12.10): `first_seen` withholds `enable_stale_read`, because a transform that rewrites an earlier message when a later one arrives cannot coexist with relying on the provider's cached prefix. The gate is insurance for the shapes where the transform can fire — OpenAI string content, single-block messages — and its reach is narrower than the rule suggests. Making the transform causal, rather than switching it off, is unbuilt. |

## 20. Open questions

1. **Handle format.** `k_` + BLAKE2b-9 / base64url gives 72 bits, which is chosen
   for marker compactness rather than collision resistance — a handle is a
   lookup key inside one tenant, not a security boundary. Only the
   12-character form ships. **This item used to claim a `refs.handle_format`
   switch was "designed in (the scanner already accepts more than one shape)".
   Both halves were false** and are corrected here on review: the identifier
   appears nowhere outside this paragraph, and `refs_in` hardcodes a single
   `ref:k_` needle against a fixed `HANDLE_WIDTH`. Supporting a second shape is
   work, not a flag.
2. **Characters, not tokens, for the never-larger guard by default.** `tokens` is
   opt-in until a corpus A/B moves it (§5.13).
3. **Server-side retrieve execution on by default in the proxy**, capped at 3
   rounds and 16K injected tokens. A `client` mode exists for gateways that own
   their own tool loop.
4. **OpenAI frozen prefix is 0, and not configurable.** Compress the history,
   because the output is byte-stable turn over turn — measured on a 53-turn
   gpt-5.5 run at 49.4% removed with a 97.2% median cache-hit rate. The
   `all_but_last_turn` alternative for operators who would rather only touch the
   live zone is designed, not built (§19); until it is, this paragraph describes
   behaviour, not a choice.
5. **No `superseded` read replacement** — eliding a read that a later *read* of
   the same file replaces. The reason given has always been that it rewrites
   content further back in the history and busts prompt caches.

   That reason is sound, and it applies just as squarely to the **stale**-read
   replacement that *does* ship (a later *write* to the same path), which this
   document asserted was safe until a `first_seen` run made the conflict
   visible. Both rewrite an earlier message because of a later one; neither is
   prefix-monotonic. The shipped one is now withheld wherever the proxy is
   relying on cache stability (§12.10), and the case for building this one is
   weaker than ever.
6. **Gemini, Bedrock and Vertex are out of scope**; `api_family = "passthrough"`
   forwards them untouched.
7. **Query relevance is out of the engine and into the proxy**, which is not
   what this item used to say. It read "query relevance stays out, on the
   measurement in §18.4", and that is no longer true of the system as a whole.

   §18.4 still holds where it was measured: the engine's output must not depend
   on the question being asked, because an agent re-sends its history every turn
   under a different latest question and query-dependent bytes rewrite a prefix
   the provider is caching. `no_payload_is_query_dependent` still enforces it
   and still passes.

   What ships now is query-dependent **protection**, not query-dependent
   compression: `query_is_about` decides whether a fragment this conversation
   already elided should be kept inline for a turn that reads as asking about it
   (§12.6). It is the only recovery path a relayed websocket has, since that
   transport can offer no tool. The cache objection applies to it too — a
   promotion flips a message from elided to inline, which is a prefix rewrite —
   and the mitigation is that promotion is **sticky**: once wanted, a fragment
   stays inline, so the decision is taken once rather than re-litigated by each
   turn's wording. The measured evidence is mildly reassuring and not
   conclusive: the run with the largest inline set of ten (244,025 tokens held)
   had zero cache loss. Whether stickiness is sufficient has not been tested
   adversarially.
8. **The tools block moved the prefix the first time a handle was minted. The
   fix is shipped; it did not fix the cache loss.** `retrieve_tool = "auto"`
   used to decide from
   whether the *rewritten* body carries a handle, so `optscale_kompressor_retrieve` is
   injected on exactly the turn the engine first mints one. A tools block is
   part of the prompt prefix, so it appears mid-conversation and everything
   after it is re-read at the fresh rate.

   Measured over ten terminal-bench runs on one task: take the first turn in
   each that mints four or more handles, and **nine of the ten report a cached
   prefix of exactly 2,432 tokens** — the same number every time, because the
   preamble ahead of the tools block is the same every time. Where the preceding
   turn had not compressed, the cache had first grown to 11,648, and the drop
   registers as a 9,216-token loss. The Anthropic case is far worse and was
   already known: `the_turn_that_mints_the_first_handle_changes_the_tools_block`
   records `cache_read` going 16,101 → **0**, accounting for every one of that
   run's 19,705 excess writes. On OpenAI, at the rates solved in §12.6, it costs
   about **$0.04 a run**, roughly 2.7% of a $1.50 run. The loss happens in every
   run and is only *reported* when the previous turn happened not to compress,
   so `cache lost` in the benchmark is a floor rather than a total.

   The fix is to offer the tool from the first request of a recoverable
   conversation instead of from the first request that carries a handle, so the
   block is in the prefix from the start and never moves. **It shipped** —
   `offers_retrieve_tool` resolves `Auto` to true for any recoverable
   conversation — and the experiment below shows it does **not** fix the cache
   loss, which survives with the block absent from the problem entirely. It was
   taken for prefix stability rather than as a cure.

   **It traded against a property this codebase stated deliberately, and the
   trade was made.** `a_request_below_the_size_gate_reaches_the_upstream_byte_identical`
   asserted that "under the gate the proxy is a pipe: not a re-serialization,
   not a reordering", and injecting below the gate puts every small request
   through serde. Six tests failed on it, four of them that guarantee. The
   property is now stated as "a pipe apart from the stable tools block", and the
   test compares with the tool stripped. Anyone relying on byte-identity under
   the gate should read it as byte-identity of everything except a tools block
   that is constant for the session.

   **The bodies have now been diffed, hermetically, and they clear two of the
   three suspects** (`tests/prefixdiff.rs`). Driving the transition through the
   proxy against a recording upstream: turn one reaches the upstream
   byte-identical, turn two shares its first 391 of 393 bytes — so
   re-serialization is byte-stable and does not move a cached byte. Three shapes
   were tried for a message sent inline on one turn and elided on the next —
   under the token gate then over it, a repeat the cross-turn pass could fold,
   and a payload compressible from turn one — and in all three the conversation
   bytes survive intact. And on `openai_responses` the `tools` key is serialized
   **after** `input` (byte 2,145, past the divergence at 391), so injecting the
   retrieve tool cannot shift a conversation byte. The Anthropic test's
   reasoning, that "`tools` sits in front of `messages`", does not transfer.

   **What it cannot settle, and why.** OpenAI caches on its own rendering of the
   request, where tools are part of the prefix ahead of the conversation,
   not on our JSON key order. Nothing observable from here shows that rendering,
   so a byte diff of what we send cannot confirm or refute the tools block.
   Chasing more offline diffs is wasted effort.

   **Settled by the bytes, 2026-10-07: the tools block is the cause, and the
   two paragraphs below were wrong to clear it.** A live run captured both
   bodies at a turn where the cache fell 11,648 -> 10,624. The conversation was
   intact — 28,169 of turn two's 28,171 bytes shared, the two-byte tail being
   the array continuing. The `instructions` and `model` segments were identical.
   The agent's own tools block was 10,622 bytes on both turns. The one thing
   that changed was ours: the outgoing tools block went from **10,622 bytes with
   no `kompressor_retrieve` to 13,207 bytes with it**, the 2,585-byte difference
   being the injected tool. Nothing else moved.

   That also explains the small magnitudes. On `openai_responses` the tool sits
   after the conversation rather than before it, so injecting one invalidates a
   tail rather than the whole prefix — which is why the common early loss is
   1,024 tokens rather than everything. The larger landings (2,432, and the
   drops to zero) are not accounted for by this and may be a different thing.

   **Why the experiment below misled.** Three of the five `retrieve_tool =
   "always"` runs were clean and a fourth lost only late, which is what the
   mechanism predicts; one lost its whole prefix early, and that single
   counterexample was read as clearing the block. It was not enough evidence to
   clear anything.

   **Shipped 2026-10-07.** `Auto` now asks whether the *conversation* may ever
   carry a handle — true whenever the engine mode is `recoverable` — rather than
   whether this request already does, so the tools block is in the prompt from
   the first turn and never moves. It costs its own definition, about 2,585
   bytes, on turns with nothing to retrieve; from the second turn on that sits
   in the cached prefix at a tenth the rate.

   **The pipe guarantee is weaker now, deliberately.** Four tests asserted that
   a request the proxy does not compress reaches the upstream byte-identical —
   "under the gate the proxy is a pipe: not a re-serialization, not a
   reordering". That now reads "apart from a tools block that is the same on
   every turn", compared through a helper that strips only our own entry and
   leaves the client's own tools in the comparison, so a real mangling still
   fails. `an_encoded_request_the_engine_declines_reaches_the_upstream_byte_identical`
   keeps the original byte-identity assertion under `retrieve_tool = "never"`,
   the one state in which nothing is touched at all, and gains a second half
   checking that a rewritten body is still zstd and still the same request
   underneath.

   `the_turn_that_mints_the_first_handle_changes_the_tools_block` is now
   `..._leaves_the_tools_block_alone`. It was written as a characterisation of
   the break, with a comment saying "if this ever comes back equal, the
   injection stopped perturbing the prefix and the break described above is
   gone". It has, so the `assert_ne!` became an `assert_eq!` and the test holds
   the fix rather than the bug.

   **The experiment, as run, and why it was over-read.** With `retrieve_tool = "always"` the tool is in the prompt from the
   first request and never appears mid-conversation. The two runs disagree with
   each other and neither helps the block's case: one lost its whole prefix at
   the first-compression turn (11,648 → 0, worse than any `auto` baseline), the
   other lost nothing early at all and its cache *grew* straight through its
   first compression. A cause that is present in both and absent from the effect
   in one is not the cause.

   **A fourth suspect, also cleared: the warm-to-cold protection flip.** It was
   the best candidate left on paper — `handler.rs` lifts
   `Protection::WhileCached` when the tracker reports `PrefixCache::Cold`, which
   is a mechanism for one message being protected on one turn and compressible
   on the next, exactly the shape the losses have. It cannot fire on this
   family, for a reason worth recording so it is not proposed again:
   **`Protection::WhileCached` is set only by the Anthropic walker**, so no
   OpenAI slot ever carries it and the `Cold` branch has nothing to lift — which
   is also why every Harbor run reports `0 inside the frozen prefix`.
   `the_warm_to_cold_flip_does_not_rewrite_what_was_already_sent` drives the flip
   for real (a warm turn, then a zero read, which is the only evidence the
   tracker accepts), asserts that both numbers reached it so the test cannot
   pass vacuously, and compares the bytes across it. Nothing moves.

   **The checker under-reported for most of this investigation.**
   `cache_losses` skipped any drop whose new value was zero, because the test
   read `not now` where it meant `now is None` — so a *total* prefix loss, the
   largest there is, registered as no loss at all. Three were invisible:
   29,056 → 0, 27,008 → 0, and 11,648 → 0. Every loss figure quoted from this
   bench before 2026-10-06 is a floor. It also explains a magnitude recorded
   early on as "-29,056 total … not a multiple of 1024, probably a sum": not a
   sum, a drop to zero.

   **Two further ideas died on measurement rather than argument.** That the drop
   size tracks how much was rewritten — an eleventh run minted 4 handles and took
   the *small* drop, where 6 handles had taken the large one three times. And
   that longer idle gaps cause eviction — loss turns have a *lower* median gap
   than clean ones, 4.6s against 5.9s across 16 losses and 552 clean turns.

   **What is left is unexplained rather than narrowed.** The one standing
   observation with no account is that the landings are discrete: across sixteen
   runs the cached prefix has fallen to exactly 2,432, 10,624 or 0 and to nothing
   else, while the content producing them varies freely. Whatever moves the
   prefix moves it to one of three places.

   **Superseded**: the two consecutive prompt bodies at the transition,
   diffed. The 2,432 boundary is consistent with the tools block *and* with the
   pipe-to-re-serializer switch that happens on the same turn, and no summary
   statistic separates them. An eleventh run sharpened the question rather than
   answering it: two different fallbacks have now been seen from the same 11,648
   prefix — 2,432 and 10,624 — and a block injected at a fixed offset predicts
   one fallback, not two. Handle count does not predict which (4 handles took the
   small drop, 6 took the large one). Every conclusion in this investigation that rested
   on aggregates alone has needed correcting at least once — the attribution was
   called provider-side, then proxy-side on three points, then rejected on a
   pooled test that mixed two populations, before the split above settled the
   early case. Capture the bytes before building on it.

9. ~~Whether `stats.db` should carry a compact per-conversation hash~~ —
   **answered.** The wire offers no reliable conversation id (OpenAI Chat has
   none, Responses has `previous_response_id`, Anthropic has none), so the
   prefix tracker's own identity is used instead: schema v5 records it on every
   row and `v_conversations` groups by it (§12.10). What remains is the query
   that turns that grouping into *unique* bytes saved, rather than the windowed
   sum that over-counts.

10. **`retrieve_max_tokens_per_request` has a measured demand profile now, and
    no measured right answer.** Schema v10 (§12.5b) made refusals countable, and
    sixteen relayed codex connections on 2026-10-09 say what the number
    actually governs. Gross varies between runs because the model pulls a different
    amount of fixture (600 KB to 1.2 MB of tool results), so the comparable
    figure is how much of the gross survives retrieval:

    | budget | seen | asks | refused | returned | kept of gross |
    |---|---|---|---|---|---|
    | 16,384 | ×5 | 7 | 10–11 | 16,244–16,246 | 92.2%, 93.1% |
    | 16,384 | ×3 | 14 | 3 | 11,681 | 95.1%, 95.1% |
    | 65,536 | ×5 | 17 | 0 | 48,705–48,710 | 79.3%–79.6% |
    | 65,536 | ×3 | 18 | 0 | **60,085–60,086** | 71.2% |

    Each row is a cluster that reproduced: within a cluster the returned figures
    span at most six tokens out of ~48,700 or ~60,000 — 0.01% — across separate
    paid runs hours apart, and runs whose reports were captured in full agree on
    net saving to a single token. `kept of gross` is given per
    run where a report exists rather than averaged, because gross itself moves
    with how much fixture the model pulls. The two earliest connections predate
    schema v10, so their refusal counts come from `serve.log` rather than from
    `retrieval_events` — they are the same 11.

    Three of those connections share a gross of **exactly 208,364 tokens**
    (225,523 in, 17,159 out), which makes them a paired comparison rather than
    an average across workloads:

    | budget | asks | refused | returned | net | kept |
    |---|---|---|---|---|---|
    | 16,384 | 7 | 11 | 16,244 | 192,120 | 92.2% |
    | 65,536 | 18 | 0 | 60,085 | 148,279 | 71.2% |

    Same payload to the token, same engine, same 69 handles minted. Eleven more
    fragments delivered cost **43,841 tokens of net saving, 21 points of
    retention**. That is the trade this setting makes, with nothing else moving.

    More asks is not the same as a worse outcome, which the clusters make
    plain: the 14-ask cluster keeps **95.1%** of its gross while the 7-ask
    cluster keeps 92–93%, because what the gate admits first decides the cost.
    The 7-ask runs spend most of the budget on one 13,251-token fragment; the
    14-ask runs serve fourteen small ones for 11,681 in total. The number to
    watch is what came back, not how many times it was asked for.

    Three things are settled. The default binds hard on this workload — 10 of 17
    asks refused, and two fragments were individually larger than the whole
    budget, so at 16,384 they can never be served at any spend (`refused: 1,
    spent: 0`). Unbounded retrieval costs a fifth to nearly a third of the
    gross saving, against 5–8% at the default. And **65,536 is not a ceiling, it
    is the off switch**: demand is bimodal at 17 or 18 fragments, each cluster
    reproducing to the token across repeat runs, and the 18-fragment cluster
    tops out at exactly 60,086 — which is to the token the unbounded figure
    that motivated applying a budget to the relay at all (`piggyback.rs`,
    `the_relay_stops_restoring_once_the_connection_has_spent_its_budget`).
    Measuring the same number from an uncapped relay in 2026-09 and from a
    65,536-capped one twice in 2026-10 is the strongest available evidence that
    the cap is doing nothing.

    **It is very nearly a relay-only setting.** The same sweep with the relay
    off — codex tries a websocket, takes seven 405s, falls back to HTTPS — asked
    for **2 fragments where the relayed run asked for 17**, and kept 98.5% of
    its gross against 71–79% relayed. Same fixtures, same model, same engine.
    The cause is structural rather than incidental: the HTTP path offers a
    `optscale_kompressor_retrieve` tool and the model must choose to call it, while the
    relay can offer no tool at all (§12.5b) and so restores any handle the frame
    *mentions*. Mentioning is far cheaper than calling, so the relay generates an
    order of magnitude more retrieval and the budget is what stands in for the
    model's restraint.

    Gross is not comparable between the two transports and the 1,855,026 the
    HTTPS run reports should not be read against the relay's 235,742: HTTPS
    re-sends the whole conversation every turn, so the per-request sum counts a
    message once per turn it survives, while the relay files one row per
    connection and sees each frame once. The ask counts and the retention ratio
    are the comparable figures, and both say the same thing.

    One thing is not. **Whether the retrieved fragments were worth their price
    is unmeasured**: this sweep runs one task and scores no accuracy, so "the
    model got what it asked for" is the only claim available, and a model
    fetching speculatively looks identical to one that needed the bytes. That is
    the whole question, and no amount of re-running this sweep answers it — the
    repeat runs agree to 0.04%, so the instrument is precise and measuring the
    wrong thing.

    So the improvement is not a value, it is the instrument that would justify
    one: a sweep that scores answers, run at 16,384 / 32,768 / 65,536 over
    enough tasks to separate the arms. 32,768 is the interesting middle — above
    the largest single fragment seen (14,525), so nothing is unservable, while
    still refusing roughly half of an 18-fragment connection — but choosing it
    today would be choosing by argument, which is how this project previously
    published a 5.96% saving it had to retract.

11. **A configuration warning emitted by `serve` reaches no log.**
    `Command::Serve` calls `Config::load` and only then `serve::run`, which is
    where the subscriber is installed — so every `tracing::warn!` inside
    `apply_env` fires with nothing listening. Met head-on:
    `KOMPRESSOR__PROXY__RETRIEVE_MAX_TOKENS_PER_REQUEST=65536` is not a
    supported override, the guard for exactly that case fired, and the paid run
    it was meant to protect went ahead on the old budget and was discovered
    afterwards from the proxy's own `"budget":16384`. `check-config` shows the
    warning because it initialises tracing first, which is why the guard reads
    as working.

    This is the "accepted and silently ignored" failure the guard's own comment
    says the project has already paid four Harbor runs for, alive on the one
    command where it costs money.

    It is not fixed by moving the init earlier, which is the obvious move and
    the wrong one: `init_logging` takes `&config.log`, so the subscriber's level
    and format are themselves configuration and cannot be installed before the
    file is read. Nor by installing a provisional subscriber first — `try_init`
    deliberately keeps whichever one won, so the configured level and format
    would be silently discarded, trading one invisible setting for two. What
    actually works is to collect the warnings `apply_env` raises and replay them
    once the real subscriber is up, which means `apply_env` returning them
    rather than logging them, and every caller (`wrap`, `compress`, `stats`,
    `retrieve`, `bench`, `check-config`) deciding what to do with them. That is
    a signature change across six call sites to make one warning visible, which
    is why it is recorded here rather than done in passing.

12. ~~Whether the websocket relay's piggyback should replace the HTTP retrieve
    round~~ — **answered, no.** The round is expensive and the arithmetic
    invited it: 110 of 2,233 requests on the 89-task run of 2026-10-12 ran a
    retrieve loop over 181 rounds, billing 1.8x the prompt of a request without
    one, about $3.4 of $98.75.

    It cannot work. Replaying the proactive test over that run's captured
    bodies predicted **0 of 180** served retrievals, every miss for the same
    reason: the handle was in what the proxy sent upstream on 180 of 180 and in
    the client's own request on 0 of 180. The proxy mints the handle while
    compressing the request the model is answering, so there is no earlier turn
    to piggyback onto; piggyback works on the relay only because no tool is
    offered there and an unresolved handle survives across frames. §12.6 holds
    the detail, including why pre-empting a first fetch would require the
    query-dependent output §18.4 forbids.

    What is left is 94 one-off fetches that nothing can predict and 86 repeats
    that `retrieved_before` already handles. **The open question that replaces
    this one is not how to deliver a fragment more cheaply but why a run needs
    21% more turns than the cheapest comparable one** — cache reads are 45% of
    the bill and scale with turns times prefix, which is where the $12 of the
    $21 gap against OptScale Kompressor's cheapest run sits. That is an
    agent-behaviour question this proxy may not be able to answer at all.

13. **Piggyback the rounds *after* the first.** Item 12 refutes replacing the
    HTTP tool with the relay's piggyback, and it refutes only that. It measured
    the *first* fetch of a handle minted on the turn being answered, where no
    earlier turn exists to attach anything to. The second and later rounds of a
    multi-round loop are a different case, because by then the **proxy owns the
    submission**: it is building the next round itself and can attach whatever
    it likes to it.

    That is where a little over a third of the cost sits. On the 89-task run of
    2026-10-12:

    | rounds | requests | rounds spent | billed prompt |
    |---|---|---|---|
    | 1 | 67 | 67 | 3,790,647 |
    | 2 | 24 | 48 | 1,978,200 |
    | 3 | 10 | 30 | 1,127,031 |
    | 4 | 9 | 36 | 1,281,258 |

    **71 of 181 rounds (39%) are second-or-later**, and a multi-round request
    bills 102,011 prompt tokens on average against 56,577 for a single-round
    one. Collapsing them is worth roughly 2.9M tokens — about half the retrieve
    loop's excess, and unlike item 12 it needs no prediction about a future
    turn, only about the *next round of the turn in hand*.

    The signal is better here too, and is the part to design rather than assume.
    A model that has just asked for one handle has told you what it is working
    on; the other handles in the same message, or in the same tool output, are
    the obvious candidates. §18.4 does not bar this — the retrieve round is
    already a response to the question being asked, so it is query-dependent by
    construction and rewrites no cached prefix.

    What would settle it is the same measurement item 12 used, run one step
    later: for each second-or-later round, was the handle it asked for present
    in the submission that preceded it, and would a candidate rule have
    attached it? That needs the *upstream* bodies of the hidden rounds, which
    the capture holds. Not yet run.


# Appendices

## A. Marker grammar

Emitted (recoverable form; aggressive omits the bracketed handle, and `[×N]`/`…[+N chars]` lose their ` ref:` tail). A `FOLD` also loses its ` ref:` tail in recoverable mode when the occurrences it collapsed were byte-identical, since the count reconstructs them (§7.4):

```
ARRAY    ::= "... " N " similar items omitted" [" [ref:" HANDLE "]"] [" (fields: " k ("," " " k)* ")"] " ..."
CLIP     ::= "…[+" N " chars" [" ref:" HANDLE] "]"
FOLD     ::= LINE " [×" N [" ref:" HANDLE] "]"
OMIT     ::= "[... " N " " NOUN " omitted" [" [ref:" HANDLE "]"] SUFFIX "]"
             NOUN ∈ { "matches", "log lines", "diff lines", "rows", "entries", "body lines", "comment/blank lines",
                      "quoted source", "read" }
             SUFFIX ∈ { "", "; " E " ERROR, " W " WARN", " in " PATH, " across " K " further files", " of context in " PATH }
CODE_PY  ::= "# [... " N " body lines omitted from " NAME ["; retrieve: ref:" HANDLE] "]"
CODE_C   ::= "{/* [... " N " body lines omitted from " NAME ["; retrieve: ref:" HANDLE] "] */}"
REDRAW   ::= "[... " N " overwritten progress " ("line"|"lines") " resolved" [" [ref:" HANDLE "]"] "]"
STALE    ::= "[Read content stale: " PATH " [... 1 read omitted [ref:" HANDLE "]]]"
HANDLE   ::= "k_" [A-Za-z0-9_-]{12}
```


Scanner: literal `ref:` then `HANDLE` not followed by `[A-Za-z0-9_-]`, `\b` before `ref`. `has_marker` literals: `" omitted"`, `"[×"`, `" chars]"`, `"ref:k_"`. `has_envelope`: output JSON containing key `"__cols__"` (lossy key factoring). Lossless envelopes: `{"__cols__":[…],"__rows__":[…]}`, `{"__keys__":[…],"__cols__":[…],"__rows__":[…]}`, escape `{"__literal__":{…}}`.

## B. Full SQLite DDL

### B.1 `refs.db`

```sql
PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA user_version=1;
CREATE TABLE IF NOT EXISTS refs (
  tenant TEXT NOT NULL, ref TEXT NOT NULL, fragment BLOB NOT NULL, bytes INTEGER NOT NULL,
  kind TEXT NOT NULL, request_id TEXT, created_at INTEGER NOT NULL, last_accessed INTEGER NOT NULL,
  ttl_seconds INTEGER NOT NULL, max_lifetime INTEGER NOT NULL,
  PRIMARY KEY (tenant, ref)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS refs_expiry ON refs (last_accessed);
```

### B.2 `stats.db`

```sql
PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA user_version=1;

CREATE TABLE IF NOT EXISTS optimization_events (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT NOT NULL,
  surface TEXT NOT NULL CHECK (surface IN ('api','mcp','proxy','cli')),
  api_family TEXT, model TEXT, provider TEXT,
  mode TEXT NOT NULL CHECK (mode IN ('aggressive','lossless','recoverable','off')),
  tokens_in INTEGER NOT NULL, tokens_out INTEGER NOT NULL, chars_in INTEGER NOT NULL, chars_out INTEGER NOT NULL,
  messages_total INTEGER NOT NULL, messages_compressed INTEGER NOT NULL, messages_skipped INTEGER NOT NULL,
  refs_emitted INTEGER NOT NULL, ref_bytes_emitted INTEGER NOT NULL,
  content_types TEXT NOT NULL CHECK (json_valid(content_types)),        -- {"json":2,"log":1}
  protected_by_reason TEXT NOT NULL CHECK (json_valid(protected_by_reason)),
  engine_ms REAL NOT NULL);
-- Added by the migration ladder, not here: `outcome` (v2), and `skipped_by_reason`
-- + `frozen_messages` (v4). This block is the base schema and is executed as
-- written by `scripts/docs/check.py`, so a column listed twice fails the check.
CREATE INDEX IF NOT EXISTS oe_tenant_time ON optimization_events (tenant, time_ms);
CREATE INDEX IF NOT EXISTS oe_request ON optimization_events (request_id);

CREATE TABLE IF NOT EXISTS retrieval_events (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT,
  surface TEXT NOT NULL, ref TEXT NOT NULL, found INTEGER NOT NULL, expired INTEGER NOT NULL DEFAULT 0,
  ranged INTEGER NOT NULL DEFAULT 0, fragment_bytes INTEGER NOT NULL, fragment_chars INTEGER NOT NULL,
  fragment_tokens INTEGER NOT NULL, returned_chars INTEGER NOT NULL, returned_tokens INTEGER NOT NULL,
  withheld INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS re_tenant_time ON retrieval_events (tenant, time_ms);
CREATE INDEX IF NOT EXISTS re_request ON retrieval_events (request_id);
CREATE INDEX IF NOT EXISTS re_ref ON retrieval_events (tenant, ref);

CREATE TABLE IF NOT EXISTS protected_ref_events (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT,
  refs TEXT NOT NULL CHECK (json_valid(refs)), refs_total INTEGER NOT NULL,
  reasons TEXT NOT NULL CHECK (json_valid(reasons)), messages INTEGER NOT NULL, touched INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS pe_tenant_time ON protected_ref_events (tenant, time_ms);

CREATE TABLE IF NOT EXISTS proxy_requests (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT NOT NULL,
  api_family TEXT NOT NULL, model TEXT, upstream TEXT NOT NULL, status INTEGER, stream INTEGER NOT NULL,
  compression_event_id INTEGER REFERENCES optimization_events(id),
  prompt_tokens INTEGER, completion_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
  shadow_prompt_tokens INTEGER, retrieve_rounds INTEGER NOT NULL DEFAULT 0,
  retrieve_tokens_injected INTEGER NOT NULL DEFAULT 0, upstream_ms REAL, total_ms REAL, error TEXT);
CREATE INDEX IF NOT EXISTS pr_tenant_time ON proxy_requests (tenant, time_ms);
CREATE INDEX IF NOT EXISTS pr_request ON proxy_requests (request_id);

CREATE TABLE IF NOT EXISTS store_samples (time_ms INTEGER PRIMARY KEY, live_refs INTEGER NOT NULL, live_bytes INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS daily_rollups (                     -- retention-proof aggregates
  day TEXT NOT NULL, tenant TEXT NOT NULL, mode TEXT NOT NULL, surface TEXT NOT NULL,
  requests INTEGER, tokens_in INTEGER, tokens_out INTEGER, retrievals INTEGER, retrieval_hits INTEGER,
  retrieved_tokens INTEGER, PRIMARY KEY (day, tenant, mode, surface));

CREATE VIEW IF NOT EXISTS v_daily_savings AS
SELECT date(time_ms/1000,'unixepoch') AS day, tenant, mode, COUNT(*) AS requests,
       SUM(tokens_in - tokens_out) AS saved_gross, SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
       1.0 - CAST(SUM(tokens_out) AS REAL)/NULLIF(SUM(tokens_in),0) AS avg_reduction
FROM optimization_events GROUP BY day, tenant, mode;

-- `withheld` rows are asks the budget refused before the store was read, so
-- they are in neither side of the rate: see §12.5b.
CREATE VIEW IF NOT EXISTS v_retrieval_hit_rate AS
SELECT tenant, date(time_ms/1000,'unixepoch') AS day,
       SUM(1 - withheld) AS retrievals, SUM(CASE WHEN withheld = 0 THEN found ELSE 0 END) AS hits,
       SUM(CASE WHEN withheld = 0 THEN expired ELSE 0 END) AS expiries,
       SUM(withheld) AS withheld,
       CAST(SUM(CASE WHEN withheld = 0 THEN found ELSE 0 END) AS REAL)/NULLIF(SUM(1 - withheld),0) AS hit_rate,
       SUM(CASE WHEN withheld = 0 THEN returned_tokens ELSE 0 END) AS retrieved_tokens
FROM retrieval_events GROUP BY tenant, day;

CREATE VIEW IF NOT EXISTS v_request_economy AS  -- see §9.5
SELECT o.tenant, o.request_id, o.time_ms, o.mode, o.model, (o.tokens_in - o.tokens_out) AS saved_gross_est,
       COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS retrieved_tokens,
       (o.tokens_in - o.tokens_out) - COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS saved_net_est,
       p.prompt_tokens, p.cache_read_tokens, p.retrieve_rounds
FROM optimization_events o LEFT JOIN proxy_requests p ON p.tenant=o.tenant AND p.request_id=o.request_id;

CREATE VIEW IF NOT EXISTS v_type_coverage AS
SELECT tenant, j.key AS content_type, SUM(j.value) AS messages
FROM optimization_events, json_each(optimization_events.content_types) AS j GROUP BY tenant, content_type;

-- v2: name why a run removed nothing
ALTER TABLE optimization_events ADD COLUMN outcome TEXT;
CREATE INDEX IF NOT EXISTS oe_outcome ON optimization_events (tenant, outcome);

CREATE VIEW IF NOT EXISTS v_outcomes AS
SELECT tenant, date(time_ms/1000,'unixepoch') AS day,
       coalesce(outcome, 'unrecorded') AS outcome,
       COUNT(*) AS requests, SUM(tokens_in) AS tokens_in,
       SUM(tokens_in - tokens_out) AS saved
FROM optimization_events GROUP BY tenant, day, outcome;

-- v3: a rewrite the proxy threw away is not a saving
DROP VIEW IF EXISTS v_daily_savings;
CREATE VIEW v_daily_savings AS
SELECT date(time_ms/1000,'unixepoch') AS day, tenant, mode, COUNT(*) AS requests,
       SUM(tokens_in - tokens_out) AS saved_gross, SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
       1.0 - CAST(SUM(tokens_out) AS REAL)/NULLIF(SUM(tokens_in),0) AS avg_reduction
FROM optimization_events
WHERE coalesce(outcome, '') <> 'discarded'
GROUP BY day, tenant, mode;

DROP VIEW IF EXISTS v_request_economy;
CREATE VIEW v_request_economy AS
SELECT o.tenant, o.request_id, o.time_ms, o.mode, o.model, (o.tokens_in - o.tokens_out) AS saved_gross_est,
       COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS retrieved_tokens,
       (o.tokens_in - o.tokens_out) - COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS saved_net_est,
       p.prompt_tokens, p.cache_read_tokens, p.retrieve_rounds
FROM optimization_events o LEFT JOIN proxy_requests p ON p.tenant=o.tenant AND p.request_id=o.request_id
WHERE coalesce(o.outcome, '') <> 'discarded';

-- v4: name what was skipped, and how much of the prompt was frozen
ALTER TABLE optimization_events ADD COLUMN skipped_by_reason TEXT;
ALTER TABLE optimization_events ADD COLUMN frozen_messages INTEGER;

-- Skips, unrolled one row per reason, so "where did the rest go" is a query
-- rather than a JSON walk. Mirrors `v_type_mix` over `content_types`.
CREATE VIEW IF NOT EXISTS v_skip_reasons AS
SELECT tenant, j.key AS reason, SUM(j.value) AS messages
FROM optimization_events, json_each(optimization_events.skipped_by_reason) AS j
WHERE optimization_events.skipped_by_reason IS NOT NULL
GROUP BY tenant, reason;

-- v5: weigh skips in tokens, and give every request its conversation
ALTER TABLE optimization_events ADD COLUMN skipped_tokens_by_reason TEXT;
ALTER TABLE proxy_requests ADD COLUMN conversation TEXT;

CREATE INDEX IF NOT EXISTS pr_conversation ON proxy_requests (conversation, time_ms);

-- Skips with both weights side by side. Replaces the V4 view, which counted
-- messages alone; `tokens` is NULL for rows written before this migration and
-- 0 for `opaque`, where the engine has no text to measure.
DROP VIEW IF EXISTS v_skip_reasons;
CREATE VIEW v_skip_reasons AS
SELECT tenant, j.key AS reason,
       SUM(j.value) AS messages,
       SUM(COALESCE(json_extract(optimization_events.skipped_tokens_by_reason,
                                 '$.' || j.key), 0)) AS tokens
FROM optimization_events, json_each(optimization_events.skipped_by_reason) AS j
WHERE optimization_events.skipped_by_reason IS NOT NULL
GROUP BY tenant, reason;

-- One row per conversation: how many turns, and what the prefix actually cost.
-- `cache_write` is the unique prefix size because each token is written once;
-- `cache_read` is the windowed sum and grows with the square of the turns.
CREATE VIEW IF NOT EXISTS v_conversations AS
SELECT tenant, conversation,
       COUNT(*) AS turns,
       MIN(time_ms) AS started_ms,
       MAX(time_ms) AS last_ms,
       SUM(cache_write_tokens) AS cache_write,
       SUM(cache_read_tokens) AS cache_read,
       SUM(completion_tokens) AS completion
FROM proxy_requests
WHERE conversation IS NOT NULL
GROUP BY tenant, conversation;

-- v6: mark what the archive has already counted
ALTER TABLE optimization_events ADD COLUMN rolled_up INTEGER NOT NULL DEFAULT 0;
ALTER TABLE retrieval_events ADD COLUMN rolled_up INTEGER NOT NULL DEFAULT 0;
DELETE FROM daily_rollups
 WHERE EXISTS (SELECT 1 FROM optimization_events e
                WHERE e.tenant = daily_rollups.tenant
                  AND date(e.time_ms/1000,'unixepoch') = daily_rollups.day)
    OR EXISTS (SELECT 1 FROM retrieval_events r
                WHERE r.tenant = daily_rollups.tenant
                  AND date(r.time_ms/1000,'unixepoch') = daily_rollups.day);

-- v7: the billed-prompt side, which daily_rollups does not cover
CREATE TABLE IF NOT EXISTS daily_requests (
  day            TEXT    NOT NULL,
  tenant         TEXT    NOT NULL,
  requests       INTEGER NOT NULL DEFAULT 0,
  prompt_tokens  INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
  cache_write_tokens INTEGER NOT NULL DEFAULT 0,
  completion_tokens  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (day, tenant));
ALTER TABLE proxy_requests ADD COLUMN rolled_up INTEGER NOT NULL DEFAULT 0;

-- v8: conversations as a set, so the count survives repeated passes
CREATE TABLE IF NOT EXISTS daily_conversations (
  day          TEXT NOT NULL,
  tenant       TEXT NOT NULL,
  conversation TEXT NOT NULL,
  PRIMARY KEY (day, tenant, conversation));

CREATE VIEW IF NOT EXISTS v_daily_requests AS
SELECT r.day, r.tenant, r.requests, r.prompt_tokens, r.cache_read_tokens,
       r.cache_write_tokens, r.completion_tokens,
       (SELECT COUNT(*) FROM daily_conversations c
         WHERE c.day = r.day AND c.tenant = r.tenant) AS conversations
  FROM daily_requests r;

-- v9: did the proxy remove the `usage` it asked for on the client's behalf?
ALTER TABLE proxy_requests ADD COLUMN strip_usage INTEGER NOT NULL DEFAULT 0;

-- v10: `retrieval_events.withheld` -- an ask the per-request token budget
-- refused before the store was read. Neither a hit nor a miss; every rate over
-- this table excludes it (§12.5b). The migration also drops and rebuilds
-- `v_retrieval_hit_rate`, because the view reads the new column.
--
-- Written as a comment rather than as runnable DDL: the column and the rebuilt
-- view are both already in the schema block above, which is what `check_schema`
-- applies, and repeating the ALTER here would add the column twice. The gate
-- catches that, which is how this note came to be a comment.
```

## C. Configuration reference — flat environment mapping

| Flat `KOMPRESSOR_*` | Sectioned key |
|---|---|
| `MODE`, `MIN_COMPRESS_CHARS`, `MAX_ARRAY_ITEMS`, `MAX_FIELD_CHARS`, `KEEP_TAIL_LINES`, `REF_TTL_SECONDS`, `MAX_REF_BYTES`, `ENABLE_*`, `SEARCH_MAX_FILES`, `DIFF_MAX_FILES`, `CODE_MAX_BODY_LINES`, `MIN_NESTED_CHARS`, `MAX_NESTED_DEPTH`, `PROTECTED_ROLES` | `engine.<lowercase>` (booleans `true|false`, `PROTECTED_ROLES` comma list) |
| `REDIS_URL` | none — recognised only to warn; this build has no Redis backend (§8.3) |
| `URL` | n/a — the proxy is in-process |
| `MIN_TOKENS` | `proxy.min_tokens` |
| `TIMEOUT` | `proxy.compress_timeout` |
| `ENABLED=false` | `engine.mode = "off"` for the proxy |
| `LOG_COMPRESSED` | `log.payloads` (deprecated, warns) |

**The sectioned form `KOMPRESSOR__SECTION__KEY` reaches an allow-list, not every
field.** `apply_structured` matches a fixed set per section and warns on
anything else; a field that is not listed below is read from the configuration
file only. The list is short because each entry is a deliberate decision that
the setting is worth changing without an edit-and-restart, not an oversight to
be filled in.

| Section | Reachable keys |
|---|---|
| `server` | `listen`, `api_listen`, `mcp_listen`, `proxy_listen` |
| `auth` | `mode` (`api_keys` only), `secret` |
| `refs` | `path`, `backend` (`memory` only) |
| `stats` | `path`, `enabled` |
| `log` | `level`, `format`, `payloads` |
| `proxy` | `enabled`, `compress`, `websocket_relay`, `min_tokens`, `frozen_prefix`, `retrieve_tool`, `retrieve_execution`, `retrieve_max_rounds` |

**`proxy.retrieve_max_tokens_per_request` is not on that list**, which is worth
stating because it is the one most likely to be reached for: it is the budget
§20 item 10 is about, and it changes only from the configuration file. Setting
`KOMPRESSOR__PROXY__RETRIEVE_MAX_TOKENS_PER_REQUEST` warns and is ignored — and
on `serve` that warning reaches no log (§20 item 11), so a paid run went ahead
on the old value and the mistake was found afterwards from the proxy's own
`"budget":16384`. `kompressor check-config` prints both the warning and the
value actually in force, and is the way to confirm an override landed before
spending anything on it.


## D. Glossary

- **CCR** — compress-cache-retrieve: lossy view in the prompt, original stashed under a handle, retrieved on demand.
- **Handle / ref** — `k_` + 12 URL-safe chars, BLAKE2b-9 of the elided fragment's UTF-8 bytes.
- **Fragment** — exactly the bytes elided at one site (omitted items, clipped tail, folded occurrences, dropped lines), never the whole message.
- **Marker** — the visible text that announces an elision (Appendix A).
- **Accounted shrink** — a shrink that carries a marker or is provably layout-only (declared `reformat`) or is a declared reversible envelope.
- **Arbitration** — computing two readings (segmenter vs whole; folded vs plain; echo-elided vs plain) and keeping the shorter, with the loser's refs rolled back.
- **Frozen prefix / floor** — messages the provider prompt cache holds; never rewritten.
- **Live zone** — the newest blocks the model answers against; the only ones whose bytes may change without cache loss.
- **Protected** — a message withheld from compression: `frozen`, `flagged` (`cache_control`/`protected`), or `role:<r>`.
- **Stranded marker** — a marker whose fragment expired because nothing re-stashed or touched it.
- **Coverage / claimed / billed** — corpus metrics: share of eligible bytes removed; share touched at any ratio; coverage weighted by re-send cost (`1 + (requests−1)·0.1`).

## E. Data model reference — every table, with real rows

Two SQLite files, never one. They have opposite write patterns and opposite value: `refs.db` is hot, small, and load-bearing (losing it strands markers mid-conversation); `stats.db` is append-only, unbounded, and purely observational (losing it costs you reporting and nothing else).

| Database | Default path | Written by | Rows per proxied request | Losing it |
|---|---|---|---|---|
| `refs.db` | `./data/refs.db` (`refs.path`) | engine, on every compression that elides | one per *distinct* fragment (upsert) | markers in flight become unretrievable; the model sees `[ref:…]` and gets a miss |
| `stats.db` | `./data/stats.db` (`stats.path`) | the stats writer thread, off the request path | 1 `optimization_events` + 1 `proxy_requests` + 1 per retrieval | reporting only; compression is unaffected |

Both carry `PRAGMA user_version` and an integer-keyed migration ladder; both are `journal_mode=WAL`. Neither is on the critical path for correctness — a store error is a warning, and a stats error is dropped.

Every example row below is real, taken from `runs/harbor_smoke_recoverable/2026-09-18__15-31-42` (35 proxied requests, recoverable mode, 52.7% of engine-visible tokens removed).

---

### E.1 `refs.db` — the fragment store

One table. It is a content-addressed key-value store with a TTL, nothing more.

#### `refs`

| Column | Type | Meaning |
|---|---|---|
| `tenant` | TEXT | tenant scope; `''` for single-tenant deployments. Part of the primary key, so two tenants never share a fragment even when the bytes are identical |
| `ref` | TEXT | the 12 url-safe base64 chars of the handle — **stored without the `k_` prefix** |
| `fragment` | BLOB | exactly the bytes that were elided at one site. UTF-8, uncompressed, no envelope |
| `bytes` | INTEGER | `length(fragment)`; denormalised so the size sweeps never read blobs |
| `kind` | TEXT | the storage kind recorded at put time (`text` for everything the proxy stores today) — for debugging, not for routing |
| `request_id` | TEXT | the request that first minted this handle; kept on re-put via `coalesce` |
| `created_at` | INTEGER | unix **seconds**, set once and never moved by a re-put |
| `last_accessed` | INTEGER | unix seconds, bumped by a read (`touch`), by `touch_many`, and by a re-put |
| `ttl_seconds` | INTEGER | the **idle** window; the row is live while `last_accessed + ttl_seconds >= now` |
| `max_lifetime` | INTEGER | absolute cap in seconds from `created_at`; a touch cannot resurrect a row past it |

Primary key `(tenant, ref)`, `WITHOUT ROWID`. One index, `refs_expiry ON (last_accessed)`, for the purge sweep.

A real row:

```
tenant        = ''
ref           = QvsgHSsqBwOt          <- the prompt says [ref:k_QvsgHSsqBwOt]
fragment      = <736 bytes>
bytes         = 736
kind          = text
request_id    = kp-01a0b44f840a-00000022
created_at    = 1789731223            <- seconds, not milliseconds
last_accessed = 1789731505            <- bumped when the model retrieved it
ttl_seconds   = 14400                 <- 4 h idle window
max_lifetime  = 115200                <- 32 h hard cap (8 × ttl)
```

The blob is literally the elided text — `hex(substr(fragment,1,32))` is `202020202222220A2020...`, which is `"    \"\"\"\n    Retry utility for a "`. And the key is derivable from the value:

```python
blake2b(fragment, digest_size=9).digest()          # 42fb201d2b2a0703ad
base64.urlsafe_b64encode(...).rstrip('=')          # QvsgHSsqBwOt  == refs.ref
```

That property is what makes a run auditable after the fact: re-compress the original payloads with the same config and the handles must match byte for byte, or the reconstruction differs.

It is also why the table is far smaller than the handle count suggests. In the reference run the engine minted **1,229 handles** but the table holds **64 rows / 192,250 bytes** — the same fragments re-stashed on every turn, which is exactly what keeps the compressed prefix byte-stable for the provider cache.

```sql
-- the fragments that are actually alive, largest first
SELECT ref, bytes, kind, datetime(last_accessed,'unixepoch') AS seen
FROM refs
WHERE last_accessed + ttl_seconds >= strftime('%s','now')
ORDER BY bytes DESC LIMIT 10;
```

---

### E.2 `stats.db` — the event log

Eight tables, eight views. Tables are append-only facts; views are the interpretations (§9.5).

#### `optimization_events` — one row per compression call

| Column | Type | Meaning |
|---|---|---|
| `id` | INTEGER PK | rowid |
| `time_ms` | INTEGER | unix **milliseconds** (unlike `refs.db`, which uses seconds) |
| `tenant` | TEXT | tenant scope |
| `request_id` | TEXT | the join key to `proxy_requests` and `retrieval_events` |
| `surface` | TEXT | `api` \| `mcp` \| `proxy` \| `cli` (CHECK-constrained) |
| `api_family` | TEXT | `openai_chat`, `openai_responses`, `anthropic_messages`, … ; empty off the proxy |
| `model`, `provider` | TEXT | as seen on the request; may be empty |
| `mode` | TEXT | `aggressive` \| `lossless` \| `recoverable` \| `off` (CHECK-constrained) |
| `tokens_in` / `tokens_out` | INTEGER | estimator tokens over the **engine-visible** text, before and after |
| `chars_in` / `chars_out` | INTEGER | the same in characters — the number to trust when arguing about ratios, since tokens are estimated |
| `messages_total` | INTEGER | messages the engine looked at |
| `messages_compressed` | INTEGER | messages it actually changed |
| `messages_skipped` | INTEGER | protected, too short, or left alone because compressing was not shorter |
| `refs_emitted` | INTEGER | handles minted in this call (**not** distinct fragments) |
| `ref_bytes_emitted` | INTEGER | bytes handed to the store in this call |
| `content_types` | TEXT (JSON) | `{"config":3,"listing":5,…}` — detector outcomes, counted **once per message** per type |
| `protected_by_reason` | TEXT (JSON) | `{"role:assistant":30}` — why messages were withheld |
| `engine_ms` | REAL | wall time inside the engine |
| `outcome` | TEXT | why the run removed nothing, or `compressed` when it did: `compressed` \| `prefix_frozen` \| `all_protected` \| `discarded` \| `no_compressible_content`. NULL on rows written before the column existed, which `v_outcomes` reports as `unrecorded` rather than folding into a real outcome |

Indexes: `(tenant, time_ms)`, `(request_id)`.

A real row — the last request of the run:

```
id                  = 36
time_ms             = 1789731505164
tenant              = ''
request_id          = kp-01a0b44f840a-00000022
surface             = proxy
api_family          = openai_responses
model               = gpt-5.5
mode                = recoverable
tokens_in           = 62248        tokens_out = 32021
chars_in            = 208049       chars_out  = 108973
messages_total      = 84   messages_compressed = 22   messages_skipped = 62
refs_emitted        = 61   ref_bytes_emitted   = 98240
content_types       = {"config":3,"listing":5,"search":5,"source_code":3,"text":6}
protected_by_reason = {"role:assistant":30,"role:developer":1}
engine_ms           = 25.615194
```

Read that row as a sentence: of 84 messages, 62 were left alone (30 of them because they were assistant turns), 22 were rewritten, and the 22 gave back half the characters in 25 ms.

Note `messages_compressed` (22) against `content_types` summing to 22 — the JSON counts each type at most once per message, so it sums to the compressed count only when no message mixed types. It is a coverage histogram, not a call counter.

#### `retrieval_events` — one row per `optscale_kompressor_retrieve` execution

| Column | Type | Meaning |
|---|---|---|
| `id` | INTEGER PK | rowid |
| `time_ms` | INTEGER | unix milliseconds |
| `tenant`, `request_id`, `surface` | TEXT | `request_id` is the request that *asked*, which is usually not the one that minted the handle |
| `ref` | TEXT | **with** the `k_` prefix — the opposite of `refs.ref` (see E.4) |
| `found` | INTEGER | 1 on a hit, 0 on a miss |
| `expired` | INTEGER | 1 when a row existed but was past its window — a stranded marker, distinct from never-existed |
| `ranged` | INTEGER | 1 when the caller asked for a line/pointer/byte window rather than the whole fragment |
| `fragment_bytes` / `fragment_chars` / `fragment_tokens` | INTEGER | the size of the stored fragment |
| `returned_chars` / `returned_tokens` | INTEGER | what was actually handed back — smaller than the fragment when `ranged=1` or when the retrieve cap bit |
| `withheld` | INTEGER | 1 when `proxy.retrieve_max_tokens_per_request` refused the ask **before the store was read** (v10). Not a miss: nobody looked, and the fragment may well have been there. Every size on such a row is 0, because nothing was read |
| `rolled_up` | INTEGER | 1 once the row's day has been archived into `daily_rollups` (v6) |

Indexes: `(tenant, time_ms)`, `(request_id)`, `(tenant, ref)`.

`found = 0` therefore means two different things depending on `withheld`: the
store was asked and could not answer (a miss — an expiry or an invented handle),
or it was never asked (a refusal — the budget was spent). Every rate computed
over this table excludes the second, and reports the count beside the rate
rather than inside it; `v_retrieval_hit_rate`, `kompressor stats retrievals`,
`stats summary`, the HTML report and the Harbor and sweep reports all do this.
Folding them together invents a store failure that did not happen, which is
what each of those did until 2026-10-09 — the Harbor run of 15:56 printed
`12 (11 hits)` for twelve asks of which eleven were put to the store and all
eleven hit.

A real row:

```
id = 9   time_ms = 1789731289355   tenant = ''
request_id = kp-01a0b44c281c-0000000a   surface = proxy
ref = k_1t8A6UooSDFh
found = 1   expired = 0   ranged = 0
fragment_bytes = 2613   fragment_chars = 2613   fragment_tokens = 892
returned_chars = 2613   returned_tokens = 892
```

`returned_tokens` is the debit side of the ledger: the whole point of `v_request_economy` is that gross savings mean nothing until you subtract it. In the reference run it was 5,925 tokens against 676,791 removed — 0.88%.

#### `protected_ref_events` — markers sighted inside protected content

| Column | Type | Meaning |
|---|---|---|
| `id` | INTEGER PK | rowid |
| `time_ms`, `tenant`, `request_id` | | as above |
| `refs` | TEXT (JSON) | array of handles seen, **with** the `k_` prefix |
| `refs_total` | INTEGER | how many |
| `reasons` | TEXT (JSON) | the protection reasons of the messages that carried them |
| `messages` | INTEGER | how many protected messages carried at least one marker |
| `touched` | INTEGER | how many handles the TTL refresh (§8.4) then touched |

A row is written **only** when a message the engine refused to compress contains a marker — an assistant turn quoting a `ref:k_…`, or a frozen prefix. In the reference run the table is **empty**: every marker lived in tool output, which the engine re-compresses rather than protects, so the TTL was refreshed by re-put instead. An empty table here is normal for agent traffic and is not evidence that protection is off — `optimization_events.protected_by_reason` is where you look for that.

#### `proxy_requests` — one row per proxied completion

| Column | Type | Meaning |
|---|---|---|
| `id` | INTEGER PK | rowid |
| `time_ms` | INTEGER | unix ms, written when the request *finished* |
| `tenant`, `request_id` | TEXT | join key to `optimization_events` |
| `api_family` | TEXT | the detected wire format |
| `model` | TEXT | as sent |
| `upstream` | TEXT | the base URL the request was forwarded to |
| `status` | INTEGER | upstream HTTP status; NULL when the request never got one |
| `stream` | INTEGER | 1 for SSE |
| `compression_event_id` | INTEGER | FK to `optimization_events(id)` — **always NULL in practice**, see below |
| `prompt_tokens`, `completion_tokens` | INTEGER | from the provider's usage block |
| `cache_read_tokens`, `cache_write_tokens` | INTEGER | provider cache accounting; the ratio of `cache_read_tokens` to `prompt_tokens` is the single best health signal for byte-determinism |
| `shadow_prompt_tokens` | INTEGER | reserved for the uncompressed prompt's billed size; **never written today** (the proxy passes `None`), so always NULL |
| `retrieve_rounds` | INTEGER | how many extra upstream round trips the retrieve tool cost. Reads `proxy.retrieve_max_rounds + 1` on a request that spent its budget — the refusal that tells the model so is a round trip too |
| `retrieve_tokens_injected` | INTEGER | tokens put back by those rounds |
| `upstream_ms`, `total_ms` | REAL | time at the upstream and end to end |
| `error` | TEXT | set when the request failed or was abandoned; carries the abandonment note when a client disconnects mid-stream |

Indexes: `(tenant, time_ms)`, `(request_id)`.

A real row:

```
id = 35   time_ms = 1789731505191   tenant = ''
request_id = kp-01a0b44f840a-00000022
api_family = openai_responses   model = gpt-5.5
upstream = https://api.openai.com   status = 200   stream = 1
compression_event_id = NULL
prompt_tokens = 52479   completion_tokens = 402
cache_read_tokens = 50560   cache_write_tokens = NULL
retrieve_rounds = 0   retrieve_tokens_injected = 0
upstream_ms = NULL   total_ms = 6501.493328
```

Two absences are deliberate rather than broken:

- **`compression_event_id` is never populated.** The proxy can finish a request before the compression event it queued has been committed, so the rowid it would point at may not exist yet. Every query that needs the link joins on `(tenant, request_id)`, which both rows carry from the start. The column is kept because a future synchronous writer could fill it without a migration.
- **`upstream_ms` is NULL on streams.** There is no single moment a streamed response is "received"; `total_ms` covers it.

`cache_read_tokens / prompt_tokens` = 50,560 / 52,479 = 96.3% on this row. Across the run it was 1,226,496 / 1,323,253 = **92.7%**, which is the empirical proof that the compressed prefix was byte-identical turn over turn. A drop in that ratio is the first symptom of a non-deterministic compressor.

#### `store_samples` — periodic size of the fragment store

| Column | Type | Meaning |
|---|---|---|
| `time_ms` | INTEGER PK | unix ms of the sample |
| `live_refs` | INTEGER | rows not past their TTL |
| `live_bytes` | INTEGER | sum of `bytes` over those rows |

The writer accepts `Event::StoreSample` and the `INSERT OR REPLACE` is deliberate — two samples in the same millisecond are the same measurement, and a primary-key conflict here would abort a whole batch of unrelated events. **Nothing emits that event outside the crate's own tests**, so the table is empty in every real deployment. Until something schedules a sweep, use `SELECT count(*), sum(bytes) FROM refs` against `refs.db` for live store size; §E.1 has the query.

#### `daily_rollups` — retention-proof aggregates

| Column | Type | Meaning |
|---|---|---|
| `day`, `tenant`, `mode`, `surface` | TEXT | composite primary key |
| `requests` | INTEGER | compressions that day |
| `tokens_in`, `tokens_out` | INTEGER | summed |
| `retrievals`, `retrieval_hits`, `retrieved_tokens` | INTEGER | the debit side |

Written by `StatsWriter::rollup` before `StatsWriter::prune` deletes raw events, so the long-run numbers survive the pruning. Recomputing a day is safe: the job deletes that day's rollups first.

**Neither is scheduled.** `stats.retention_days` (default 90) is plumbed into the writer's config and `prune`/`rollup` are implemented and tested, but no timer calls them, so today `stats.db` grows without bound and this table stays empty. Run them by hand, or treat the raw event tables as the only source and prune the file yourself. Retrievals are attributed to the mode of the compression sharing their `request_id`, and to `unknown` when there is none — dropping the unattributable ones would quietly delete debits and leave only credits, making the preserved aggregate flattering rather than merely incomplete. Empty until the job has run once.

---

### E.3 Views

| View | Grain | Answers |
|---|---|---|
| `v_daily_savings` | day × tenant × mode | "how much did we remove, and what fraction was that?" |
| `v_retrieval_hit_rate` | day × tenant | "are markers still resolvable, or are fragments expiring under us?" |
| `v_request_economy` | one request | "what did this request save *net* of what retrieval put back?" |
| `v_type_coverage` | tenant × content type | "what kind of text is this workload actually made of?" |
| `v_outcomes` | day × tenant × outcome | "when nothing was removed, why not?" |

Real rows from the reference run:

```
v_daily_savings       day=2026-09-18 mode=recoverable requests=36
                      saved_gross=715578  tokens_in=1323220  tokens_out=607642
                      avg_reduction=0.5408

v_retrieval_hit_rate  day=2026-09-18 retrievals=9 hits=9 expiries=0
                      hit_rate=1.0  retrieved_tokens=5925

v_type_coverage       config 61 | listing 94 | log 1 | search 80
                      source_code 84 | text 89

v_request_economy     request_id=kp-01a0b44b4692-00000003  mode=recoverable
                      saved_gross_est=896  retrieved_tokens=175  saved_net_est=721
                      prompt_tokens=28362  cache_read_tokens=27392  retrieve_rounds=1
```

Three cautions when quoting these:

1. `v_daily_savings` counts **every** surface, so a CLI or API call made against the same database lands in the same day bucket as proxied traffic. The reference run shows 36 requests here against 34 proxied ones, because two were `surface = api` warmups.
2. `saved_gross_est` is in estimator tokens over engine-visible text, not billed tokens. It is not `prompt_tokens` before minus after; the prompt also contains material the engine never looked at.
3. `avg_reduction` is a ratio of sums, not a mean of ratios. That is deliberate — a mean of per-request ratios over-weights tiny requests — but it means one enormous request can move the number on its own.

---

### E.4 Joining the two databases

They are separate files, so a join needs `ATTACH` — and one normalisation:

```sql
ATTACH './data/refs.db' AS r;

-- which retrieved handles are still resolvable?
SELECT e.ref, e.returned_tokens, f.bytes, f.last_accessed
FROM retrieval_events e
LEFT JOIN r.refs f
       ON f.tenant = e.tenant
      AND f.ref    = ltrim(e.ref, 'k_')      -- NOT a no-op: see below
WHERE e.found = 1;
```

**`retrieval_events.ref` carries the `k_` prefix; `refs.ref` does not.** `RefId` stores the 12-character body and adds `k_` in its `Display` impl; the store binds `id.body()`, while the stats event is built from `ref.to_string()`. A naive equality join returns zero rows and looks like total fragment loss. This bit the run-5 analysis before the cause was found, so it is worth stating plainly rather than leaving to be rediscovered.

(`ltrim(x,'k_')` strips any leading `k` or `_` characters, which is safe here because the base64 body never starts with one after the prefix is gone; `substr(e.ref, 3)` is the stricter form.)

The same asymmetry applies to `protected_ref_events.refs`, whose JSON array elements are prefixed.
