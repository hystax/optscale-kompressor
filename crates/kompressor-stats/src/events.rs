//! The event vocabulary written to `stats.db`.
//!
//! Every event carries `time_ms` — the **request start**, taken by the caller
//! before any work begins, never the moment the writer thread got around to
//! inserting the row. The reference service stamped rows at insert time from a
//! background task, so under load a burst of requests landed in the following
//! minute and the optimization histogram disagreed with the requests that
//! produced it: a tight window could show retrievals with no compressions, or
//! savings attributed to a minute in which nothing was compressed. Buckets are
//! only meaningful if the clock reading belongs to the request, not to the
//! queue.

use kompressor_engine::types::{Kind, Mode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Which entry point produced an event.
///
/// This is the *work mode*, not the compression mode: the same tenant can
/// compress through the HTTP API, the MCP tools, the in-process proxy and the
/// CLI, and the four have very different traffic shapes. Splitting savings by
/// surface is the cheapest way to find out that, say, all the retrieval debit
/// comes from the proxy's agent loop.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// The HTTP compression API.
    Api,
    /// The MCP server's tools.
    Mcp,
    /// The in-process LLM proxy.
    Proxy,
    /// The command-line client.
    Cli,
}

impl Surface {
    /// The spelling stored in the `surface` column, which a CHECK constraint
    /// restricts to exactly these four.
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Api => "api",
            Surface::Mcp => "mcp",
            Surface::Proxy => "proxy",
            Surface::Cli => "cli",
        }
    }

    /// Parse the stored spelling back. `None` for anything else.
    pub fn parse(s: &str) -> Option<Surface> {
        match s {
            "api" => Some(Surface::Api),
            "mcp" => Some(Surface::Mcp),
            "proxy" => Some(Surface::Proxy),
            "cli" => Some(Surface::Cli),
            _ => None,
        }
    }
}

impl fmt::Display for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One compression of one request: the credit side of the token economy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptimizationEvent {
    /// Request start in milliseconds since the Unix epoch (see the module doc).
    pub time_ms: i64,
    /// Tenant the request was billed to.
    pub tenant: String,
    /// The join key across surfaces: the proxy stamps the same id on this
    /// event, on the refs it mints and on the retrievals it executes.
    pub request_id: String,
    /// Entry point that ran the compression.
    pub surface: Surface,
    /// Wire dialect, when one applies (`openai`, `anthropic`, …).
    pub api_family: Option<String>,
    /// Model the compressed prompt was sent to, when known.
    pub model: Option<String>,
    /// Upstream provider, when known.
    pub provider: Option<String>,
    /// Compression mode actually used, after any per-request override.
    pub mode: Mode,
    /// Estimated tokens before compression.
    pub tokens_in: u32,
    /// Estimated tokens after compression.
    pub tokens_out: u32,
    /// Characters before compression. Kept next to tokens because tokens are an
    /// estimate and characters are not, so a ratio that looks wrong can be
    /// checked against something measured.
    pub chars_in: u64,
    /// Characters after compression.
    pub chars_out: u64,
    /// Messages in the request.
    pub messages_total: u32,
    /// Messages the engine rewrote.
    pub messages_compressed: u32,
    /// Messages left untouched (too small, protected, or declined).
    pub messages_skipped: u32,
    /// Fragment handles minted for this request.
    pub refs_emitted: u32,
    /// Bytes stashed behind those handles. This is the size of the liability
    /// the store now carries, and the upper bound on what retrieval can claw
    /// back.
    pub ref_bytes_emitted: u64,
    /// Messages per detected content type, with mixed bodies already unfolded
    /// into their constituent types.
    pub content_types: BTreeMap<String, u32>,
    /// Messages left alone per protection reason (`frozen`, `flagged`,
    /// `role:<r>`).
    pub protected_by_reason: BTreeMap<String, u32>,
    /// Why each skipped message was skipped. A partition of `messages_skipped`,
    /// not a sample of it — see `CompressStats::skipped_by_reason`.
    pub skipped_by_reason: BTreeMap<String, u32>,
    /// The same partition weighted by tokens — what a count cannot rank.
    /// `opaque` is necessarily 0: the engine holds no text for such a message,
    /// so 0 means "invisible from here", not "empty".
    pub skipped_tokens_by_reason: BTreeMap<String, u32>,
    /// Messages withheld because they sit inside the provider's cached prefix.
    /// Computed by the engine since the freeze existed; written since v4.
    pub frozen_messages: u32,
    /// Wall time the engine spent, in milliseconds.
    pub engine_ms: f64,
    /// Why the run removed nothing, or `compressed` when it did.
    ///
    /// `None` only for an event built by a caller that predates the field;
    /// the view reports those as `unrecorded` rather than folding them into a
    /// real outcome.
    pub outcome: Option<String>,
}

impl OptimizationEvent {
    /// An empty event for `request_id` at `time_ms`; fill in the counters.
    ///
    /// A twenty-field literal in every call site is how a caller ends up
    /// silently shifting `messages_compressed` into `messages_skipped`, so the
    /// constructor takes only the identity and leaves the counters at zero.
    pub fn new(
        time_ms: i64,
        tenant: impl Into<String>,
        request_id: impl Into<String>,
        surface: Surface,
        mode: Mode,
    ) -> OptimizationEvent {
        OptimizationEvent {
            time_ms,
            tenant: tenant.into(),
            request_id: request_id.into(),
            surface,
            api_family: None,
            model: None,
            provider: None,
            mode,
            tokens_in: 0,
            tokens_out: 0,
            chars_in: 0,
            chars_out: 0,
            messages_total: 0,
            messages_compressed: 0,
            messages_skipped: 0,
            refs_emitted: 0,
            ref_bytes_emitted: 0,
            content_types: BTreeMap::new(),
            protected_by_reason: BTreeMap::new(),
            skipped_by_reason: BTreeMap::new(),
            skipped_tokens_by_reason: BTreeMap::new(),
            frozen_messages: 0,
            engine_ms: 0.0,
            outcome: None,
        }
    }

    /// Credit `messages` more messages to `kind` in [`Self::content_types`].
    ///
    /// Goes through [`Kind`] rather than a caller-supplied string so the type
    /// names in `content_types` are the engine's own and stay joinable with
    /// everything else that reports by type.
    pub fn record_type(&mut self, kind: Kind, messages: u32) {
        *self
            .content_types
            .entry(kind.as_str().to_string())
            .or_insert(0) += messages;
    }

    /// Credit `messages` more messages to a protection `reason`.
    pub fn record_protected(&mut self, reason: impl Into<String>, messages: u32) {
        *self.protected_by_reason.entry(reason.into()).or_insert(0) += messages;
    }
}

/// One attempt to fetch a fragment back: the debit side of the token economy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalEvent {
    /// Request start in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Tenant the retrieval was billed to.
    pub tenant: String,
    /// Request that asked, when the surface knows it. The plugin path had no
    /// join key at all, which is why the economy could only ever be reported
    /// windowed; `None` here means the same loss of precision for that row.
    pub request_id: Option<String>,
    /// Entry point that served the retrieval.
    pub surface: Surface,
    /// The handle asked for, `k_` prefix included.
    pub reference: String,
    /// Whether the store had it.
    pub found: bool,
    /// Whether the miss was an expiry — a fragment we held and let go — rather
    /// than a handle we never had. Without this split an aggressive TTL looks
    /// exactly like a client inventing handles.
    pub expired: bool,
    /// Whether the caller asked for a byte range rather than the whole
    /// fragment.
    pub ranged: bool,
    /// Size of the stored fragment in bytes.
    pub fragment_bytes: u64,
    /// Size of the stored fragment in characters.
    pub fragment_chars: u64,
    /// Estimated tokens of the whole stored fragment.
    pub fragment_tokens: u32,
    /// Characters actually handed back (smaller than the fragment for a ranged
    /// read).
    pub returned_chars: u64,
    /// Estimated tokens actually handed back. **This**, not `fragment_tokens`,
    /// is what the model pays for again, so this is what the net saving is
    /// debited by.
    pub returned_tokens: u32,
    /// Whether the per-request token budget refused this before the store was
    /// read at all.
    ///
    /// Distinct from `found = false`, which says the store did not have it.
    /// This says nobody looked: `proxy.retrieve_max_tokens_per_request` was
    /// already spent, so the model asked and was told no. The two have
    /// different cures — a miss is a TTL or an invented handle, a withholding
    /// is a budget set too low for the conversation — and reporting them as one
    /// number would have made tightening the budget look like a store fault.
    ///
    /// A withheld row carries no sizes: nothing was read, so there is nothing
    /// to measure. Every rate computed over these rows excludes them, and the
    /// count is reported beside the rate rather than folded into it.
    pub withheld: bool,
}

/// One compress that found handles inside content it was not allowed to touch.
///
/// Joining these handles against [`RetrievalEvent`] is what separates an expiry
/// from a genuine miss: a handle we recorded as protected and later failed to
/// serve is a fragment we dropped while something still pointed at it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtectedRefEvent {
    /// Request start in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Tenant the request was billed to.
    pub tenant: String,
    /// Request that carried the protected content.
    pub request_id: Option<String>,
    /// The handles seen inside protected messages.
    pub refs: Vec<String>,
    /// Why those messages were protected (`frozen`, `flagged`, `role:<r>`).
    pub reasons: Vec<String>,
    /// Protected messages in the request.
    pub messages: u32,
    /// How many of those handles had their TTL refreshed by this sighting.
    pub touched: u32,
}

/// One request through the in-process proxy, with the provider's own numbers.
///
/// The billed prompt size belongs here rather than in [`OptimizationEvent`]
/// because it is measured by the provider, not estimated by us. Reporting the
/// two side by side is the only way a reader can see how much of an estimated
/// saving the provider had already discounted as a cache read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProxyRequestEvent {
    /// Request start in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Tenant the request was billed to.
    pub tenant: String,
    /// Join key shared with the compression and retrieval events.
    pub request_id: String,
    /// Wire dialect the client spoke.
    pub api_family: String,
    /// Model requested.
    pub model: Option<String>,
    /// Upstream base URL the request was forwarded to.
    pub upstream: String,
    /// HTTP status returned to the client, absent if the request never got one.
    pub status: Option<u16>,
    /// Whether the response was streamed.
    pub stream: bool,
    /// Whether the proxy removed the `usage` it had asked for itself.
    ///
    /// The proxy adds `include_usage` when the client did not, so the retrieve
    /// loop can account for rounds the client never sees, and then takes it
    /// back out of the response. For such a request the client has no usage to
    /// report, which is one way an agent's self-reported totals can fall short
    /// of what the proxy measured on the wire.
    pub strip_usage: bool,
    /// Billed prompt tokens reported by the provider.
    pub prompt_tokens: Option<u32>,
    /// Billed completion tokens reported by the provider.
    pub completion_tokens: Option<u32>,
    /// Prompt tokens served from the provider's cache, at a fraction of the
    /// price.
    pub cache_read_tokens: Option<u32>,
    /// Prompt tokens written into the provider's cache.
    pub cache_write_tokens: Option<u32>,
    /// The part of `cache_write_tokens` billed as one-hour cache entries, when
    /// the provider reports the split (Anthropic's `cache_creation`).
    pub cache_write_1h_tokens: Option<u32>,
    /// What the prompt would have billed uncompressed, when a shadow count was
    /// taken.
    pub shadow_prompt_tokens: Option<u32>,
    /// Retrieval rounds the agent loop spent inside this one request.
    /// Which conversation this request belongs to, as the prefix tracker's
    /// own fingerprint (hex).
    ///
    /// Without it the savings headline cannot be priced. `tokens_in` and
    /// `tokens_out` are summed per request, so a message compressed on turn
    /// ten is counted again on every turn through the end of the session and
    /// the total over-counts by roughly how long a message survives. Grouping
    /// by this column is what turns that windowed sum into unique bytes.
    ///
    /// `None` when nothing was frozen, which is also when there is no prefix
    /// to identify. The value is stable across turns of one conversation and
    /// across proxy restarts: it is FNV-1a over the conversation's *learned
    /// opening prefix* — the deepest opening the tracker has already seen,
    /// capped at 64 messages — never `DefaultHasher`, whose per-process seed
    /// would make two proxies disagree about which conversation a request
    /// belongs to. It was once the first frozen message alone, which collapsed
    /// 20 distinct tasks into 5 keys that then repeated across runs.
    pub conversation: Option<String>,
    pub retrieve_rounds: u32,
    /// Tokens re-injected across those rounds.
    pub retrieve_tokens_injected: u32,
    /// Time spent waiting on the upstream, in milliseconds.
    pub upstream_ms: Option<f64>,
    /// Total time in the proxy, in milliseconds.
    pub total_ms: Option<f64>,
    /// Error text, if the request failed.
    pub error: Option<String>,
}

/// A periodic measurement of what the fragment store is holding.
///
/// There is no tenant: the sweep that produces it walks the whole store, so the
/// sample is process-wide.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct StoreSample {
    /// When the sample was taken, milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Fragments alive at that moment.
    pub live_refs: i64,
    /// Bytes they occupy.
    pub live_bytes: i64,
}

/// Anything the writer can be handed.
///
/// Externally tagged — `{"optimization": {…}}` — rather than carrying a `type`
/// field. An internal tag would read better on the wire but cannot survive a
/// round trip here: internal tagging buffers the content first, and the
/// workspace builds `serde_json` with `arbitrary_precision`, under which a
/// buffered number is a map. `engine_ms` then fails to deserialize with
/// "invalid type: map, expected f64".
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// A compression happened.
    Optimization(OptimizationEvent),
    /// A fragment was asked for.
    Retrieval(RetrievalEvent),
    /// Handles were seen in protected content.
    ProtectedRefs(ProtectedRefEvent),
    /// A request passed through the proxy.
    ProxyRequest(ProxyRequestEvent),
    /// A compression the caller then threw away.
    ///
    /// The engine shrank the messages and the serialized body still came out
    /// larger, so the original was forwarded and nothing was saved — but the
    /// optimization row has already been written by then, claiming the shrink.
    /// Measured on one Harbor run: 16 of 92 requests, 2.6% of the headline.
    /// Sent after the optimization event and processed in order, so the row it
    /// amends is always there.
    Discarded {
        /// Tenant the request resolved to.
        tenant: String,
        /// The request whose rewrite was dropped.
        request_id: String,
    },
    /// The fragment store was measured.
    StoreSample {
        /// When the sample was taken, milliseconds since the Unix epoch.
        time_ms: i64,
        /// Fragments alive at that moment.
        live_refs: i64,
        /// Bytes they occupy.
        live_bytes: i64,
    },
}

impl Event {
    /// When the event happened, for every variant.
    pub fn time_ms(&self) -> i64 {
        match self {
            Event::Optimization(e) => e.time_ms,
            Event::Retrieval(e) => e.time_ms,
            Event::ProtectedRefs(e) => e.time_ms,
            Event::ProxyRequest(e) => e.time_ms,
            // an amendment to a row that already carries its own timestamp
            Event::Discarded { .. } => 0,
            Event::StoreSample { time_ms, .. } => *time_ms,
        }
    }

    /// The tenant, or `None` for the tenant-less store sample.
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Event::Optimization(e) => Some(&e.tenant),
            Event::Retrieval(e) => Some(&e.tenant),
            Event::ProtectedRefs(e) => Some(&e.tenant),
            Event::ProxyRequest(e) => Some(&e.tenant),
            Event::Discarded { tenant, .. } => Some(tenant),
            Event::StoreSample { .. } => None,
        }
    }
}

impl From<OptimizationEvent> for Event {
    fn from(e: OptimizationEvent) -> Event {
        Event::Optimization(e)
    }
}

impl From<RetrievalEvent> for Event {
    fn from(e: RetrievalEvent) -> Event {
        Event::Retrieval(e)
    }
}

impl From<ProtectedRefEvent> for Event {
    fn from(e: ProtectedRefEvent) -> Event {
        Event::ProtectedRefs(e)
    }
}

impl From<ProxyRequestEvent> for Event {
    fn from(e: ProxyRequestEvent) -> Event {
        Event::ProxyRequest(e)
    }
}

impl From<StoreSample> for Event {
    fn from(s: StoreSample) -> Event {
        Event::StoreSample {
            time_ms: s.time_ms,
            live_refs: s.live_refs,
            live_bytes: s.live_bytes,
        }
    }
}
