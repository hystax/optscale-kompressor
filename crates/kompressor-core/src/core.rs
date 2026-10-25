//! The operations themselves, shared by every surface.
//!
//! An HTTP call, a tool call and a proxied completion differ in how they are
//! framed and in nothing else: each resolves a tenant, compresses a
//! conversation or fetches a fragment back, retains what it elided, and records
//! what it did. Putting that here means the three cannot drift — a rule added
//! for one applies to all of them — and it keeps the surfaces down to
//! translation.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use kompressor_engine::messages::{
    CompressStats, Message, Outcome, PrefixCache, RequestOptions, compress_messages,
};
use kompressor_engine::options::EngineOptionsPatch;
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::refs::RefId;
use kompressor_engine::types::{Context, ForcedType, Kind};
use kompressor_engine::{EngineOptions, Mode, RefCollector};
use kompressor_stats::events::{
    Event, OptimizationEvent, ProtectedRefEvent, RetrievalEvent, Surface,
};
use kompressor_stats::writer::StatsSink;
use kompressor_store::types::{Lookup, PutMeta, RefStore, Tenant};

use crate::auth::{Authenticator, Identity};
use crate::config::Config;
use crate::retrieve::{RetrieveRange, Retrieved, slice};

/// Everything a surface needs, built once at startup.
pub struct AppCore {
    config: Config,
    auth: Authenticator,
    refs: Arc<dyn RefStore>,
    stats: StatsSink,
}

/// What one compression run produced, beyond the mutated messages.
pub struct CompressOutcome {
    pub stats: CompressStats,
    /// The mode actually used, after any per-tenant or per-request override.
    pub mode: Mode,
}

/// A finished compression that has not been published.
///
/// Holds the elided fragments and the originals to fall back to, so that
/// [`AppCore::publish`] can retain, revert and file in one step — or so that a
/// caller that no longer wants the result can drop it and leave no trace. See
/// [`AppCore::compress_only`].
pub struct Computed {
    pub stats: CompressStats,
    refs: RefCollector,
    original: Option<Vec<Message>>,
    time_ms: i64,
    engine_ms: f64,
}

/// Where a compression came from, for attribution.
#[derive(Clone, Debug, Default)]
pub struct RequestInfo<'a> {
    pub request_id: &'a str,
    pub api_family: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl AppCore {
    pub fn new(config: Config, refs: Arc<dyn RefStore>, stats: StatsSink) -> AppCore {
        let auth = Authenticator::new(config.auth.clone());
        AppCore {
            config,
            auth,
            refs,
            stats,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn auth(&self) -> &Authenticator {
        &self.auth
    }

    pub fn refs(&self) -> &Arc<dyn RefStore> {
        &self.refs
    }

    pub fn stats(&self) -> &StatsSink {
        &self.stats
    }

    /// The engine options for this identity and request.
    ///
    /// Precedence runs outward from the request: an explicit per-request mode
    /// beats a tenant's configured one, which beats the deployment default.
    pub fn options_for(
        &self,
        identity: &Identity,
        patch: Option<&EngineOptionsPatch>,
    ) -> EngineOptions {
        let mut options = self.config.engine.clone();
        if let Some(mode) = identity.mode {
            options.mode = mode;
        }
        if let Some(patch) = patch {
            options = options.merged(patch);
        }
        options
    }

    /// Compress a conversation in place, retain what it elided, and record it.
    ///
    /// The parameter list is long because each one is a separate decision a
    /// caller has to make; folding them into one struct would hide that behind
    /// a default and make a forgotten frozen prefix silent.
    #[allow(clippy::too_many_arguments)]
    pub fn compress(
        &self,
        identity: &Identity,
        messages: &mut [Message],
        request: &RequestInfo<'_>,
        options: &EngineOptions,
        frozen_message_count: usize,
        context: &str,
        surface: Surface,
        // Whether the provider is still serving that prefix from cache. The
        // caller decides, because only the caller has seen what the provider
        // reported last time; `Warm` is the answer for one that cannot tell.
        prefix_cache: PrefixCache,
        // Handles the model has already fetched back in this conversation.
        // A caller with no conversation of its own passes `&[]`; only the
        // proxy, which runs the retrieve loop, has anything else to say.
        keep_inline: &[RefId],
        // How many distinct requests asked for each of those handles. The
        // engine weighs a promotion's one-off prefix rewrite against it; an
        // empty slice prices every candidate at a single demand.
        fetches: &[(RefId, u32)],
        // What share of billed prompt tokens this provider has served from
        // cache, per thousand. Sets the price of a retrieve round against a
        // rewritten prefix. A caller that has measured nothing sends 1000, the
        // reading that demands most before rewriting anything.
        cache_hit_per_mille: u32,
    ) -> CompressOutcome {
        let started = Instant::now();
        let time_ms = now_ms();
        let mut refs = options.collector();
        let original = (options.mode == Mode::Recoverable).then(|| messages.to_vec());
        let req = RequestOptions {
            frozen_message_count,
            context,
            // Guessing `Cold` on a live cache costs a re-read of everything
            // after the first rewritten byte; guessing `Warm` on a dead one
            // only leaves a saving unclaimed. See `messages::PrefixCache`.
            prefix_cache,
            keep_inline,
            fetches,
            cache_hit_per_mille,
            // This surface sees messages alone; nothing outside them to price.
            outside_tokens: 0,
            cache_pricing: None,
        };
        let computed = Computed {
            stats: compress_messages(messages, options, &req, &mut refs),
            refs,
            original,
            time_ms,
            engine_ms: started.elapsed().as_secs_f64() * 1000.0,
        };
        self.publish(identity, messages, computed, request, options, surface)
    }

    /// Compress without retaining, recording or promoting anything.
    ///
    /// The second half is [`AppCore::publish`], and the split exists because a
    /// caller can decide *after* the work finishes that it will not use the
    /// result. The proxy races compression against a deadline on a
    /// `spawn_blocking` task, and a blocking task cannot be cancelled: when
    /// the deadline won, the abandoned task ran on to completion and stored
    /// fragments nothing could fetch, filed an optimization event for an
    /// output that was never forwarded, and moved inline-promotion state that
    /// rewrote later prompts. Measured once at 16,435 saved tokens recorded
    /// after the request had already fallen back to its original body.
    ///
    /// A cancellation flag consulted inside the work would narrow that window
    /// rather than close it — the check and the publication cannot be made
    /// atomic against a deadline that fires between them. Computing here and
    /// publishing on the caller's side closes it: nothing is published until
    /// the caller has decided, and a result it abandons is simply dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn compress_only(
        &self,
        messages: &mut [Message],
        options: &EngineOptions,
        frozen_message_count: usize,
        context: &str,
        prefix_cache: PrefixCache,
        keep_inline: &[RefId],
        fetches: &[(RefId, u32)],
        cache_hit_per_mille: u32,
        // Tokens the provider bills outside the messages — system prompt and
        // tool schemas — which a retrieve round re-sends. Zero when unknown.
        outside_tokens: u32,
        // The provider's prices for a promotion, when the caller knows them.
        // `None` prices it from `cache_hit_per_mille`.
        cache_pricing: Option<kompressor_engine::messages::CachePricing>,
    ) -> Computed {
        let started = Instant::now();
        let time_ms = now_ms();
        let mut refs = options.collector();
        let original = (options.mode == Mode::Recoverable).then(|| messages.to_vec());
        let req = RequestOptions {
            frozen_message_count,
            context,
            prefix_cache,
            keep_inline,
            fetches,
            cache_hit_per_mille,
            outside_tokens,
            cache_pricing,
        };
        let stats = compress_messages(messages, options, &req, &mut refs);
        Computed {
            stats,
            refs,
            original,
            time_ms,
            engine_ms: started.elapsed().as_secs_f64() * 1000.0,
        }
    }

    /// Retain what a compression elided, then file it.
    ///
    /// Every side effect of compressing lives here: fragment retention, the
    /// fallback when retention fails, the protected-role refresh and the
    /// optimization event. A [`Computed`] that is never passed to this has
    /// touched nothing outside its own allocations.
    pub fn publish(
        &self,
        identity: &Identity,
        messages: &mut [Message],
        computed: Computed,
        request: &RequestInfo<'_>,
        options: &EngineOptions,
        surface: Surface,
    ) -> CompressOutcome {
        let Computed {
            mut stats,
            refs,
            original,
            time_ms,
            engine_ms,
        } = computed;

        // Carried over from the call this commit's restructure displaced. It
        // belongs on this side of the split rather than in `compress_only`:
        // the note is part of what gets filed, so a computed result the caller
        // abandons should not produce one. `compress_only` has no
        // `RequestInfo` to name the request with either.
        note_oversized_refusals(&mut stats, &refs, request.request_id);

        if !self.retain_fragments(identity, &refs, request.request_id, options)
            && let Some(original) = original
        {
            messages.clone_from_slice(&original);
            discard_compression_stats(&mut stats);
        }
        self.refresh_protected(identity, &stats);
        self.record_compression(identity, &stats, request, surface, time_ms, engine_ms);

        CompressOutcome {
            stats,
            mode: options.mode,
        }
    }

    /// Compress one standalone payload.
    ///
    /// Routed as a tool result, because that is what a raw payload is: the size
    /// gate decides, not a role the caller had to invent.
    pub fn compress_raw(
        &self,
        identity: &Identity,
        content: &str,
        forced: Option<ForcedType>,
        request: &RequestInfo<'_>,
        options: &EngineOptions,
        surface: Surface,
    ) -> (String, CompressOutcome) {
        let started = Instant::now();
        let time_ms = now_ms();
        let mut refs = options.collector();
        let mut applied: Vec<Kind> = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) = compress_content(
            content,
            "tool",
            options,
            forced,
            &mut refs,
            &ctx,
            &mut applied,
        );
        let engine_ms = started.elapsed().as_secs_f64() * 1000.0;

        let mut stats = CompressStats {
            tokens_in: kompressor_engine::estimate_tokens(content, "tool"),
            tokens_out: kompressor_engine::estimate_tokens(&out, "tool"),
            chars_in: content.chars().count() as u64,
            chars_out: out.chars().count() as u64,
            compressed_messages: u32::from(kind.is_some()),
            skipped_messages: u32::from(kind.is_none()),
            mode: options.mode,
            refs_emitted: refs.len() as u32,
            ref_bytes_emitted: refs.bytes(),
            router_version: "algorithmic-v2-rs",
            ..Default::default()
        };
        stats.ratio = if stats.tokens_in > 0 {
            stats.tokens_out as f32 / stats.tokens_in as f32
        } else {
            1.0
        };
        if let Some(kind) = kind {
            for applied_kind in if applied.is_empty() {
                vec![kind]
            } else {
                applied
            } {
                *stats
                    .by_type
                    .entry(applied_kind.as_str().to_string())
                    .or_insert(0) += 1;
            }
        }

        note_oversized_refusals(&mut stats, &refs, request.request_id);
        let retained = self.retain_fragments(identity, &refs, request.request_id, options);
        if !retained && options.mode == Mode::Recoverable {
            discard_compression_stats(&mut stats);
        }
        self.record_compression(identity, &stats, request, surface, time_ms, engine_ms);

        let out = if !retained && options.mode == Mode::Recoverable {
            content.to_string()
        } else {
            out.into_owned()
        };
        (
            out,
            CompressOutcome {
                stats,
                mode: options.mode,
            },
        )
    }

    /// Fetch an elided fragment back.
    /// Read a fragment without it counting as use.
    ///
    /// [`retrieve`](Self::retrieve) is the model asking: it touches the
    /// retention clock, files a `retrieval_events` row and feeds the
    /// keep-inline machinery. This is the proxy looking at its own stored
    /// bytes to make a decision about them, which is none of those things —
    /// counting it would inflate the retrieval rate with reads nobody
    /// requested and keep fragments alive because the proxy glanced at them.
    ///
    /// `None` for a miss, an expiry, a store error, or a fragment that is not
    /// UTF-8.
    pub fn peek(&self, identity: &Identity, id: &RefId) -> Option<String> {
        let tenant = Tenant::new(identity.tenant.as_str().to_string());
        match self.refs.get(&tenant, id, false) {
            Ok(Lookup::Hit(record)) => Some(record.fragment),
            _ => None,
        }
    }

    pub fn retrieve(
        &self,
        identity: &Identity,
        handle: &str,
        range: Option<&RetrieveRange>,
        request: &RequestInfo<'_>,
        surface: Surface,
    ) -> Retrieved {
        let time_ms = now_ms();
        let Some(id) = kompressor_engine::RefId::parse(handle) else {
            // A malformed handle is not a store miss, but the caller learns the
            // same thing either way and nothing is gained by telling it which.
            self.record_retrieval(
                identity,
                handle,
                &Retrieved::miss(),
                request,
                surface,
                time_ms,
            );
            return Retrieved::miss();
        };

        let lookup = self.refs.get(&identity.tenant, &id, true);
        let result = match lookup {
            Ok(Lookup::Hit(record)) => slice(
                &record.fragment,
                range,
                self.config.retrieve.max_retrieve_chars,
            ),
            Ok(Lookup::Expired { .. }) => Retrieved::expired(),
            Ok(Lookup::Miss) => Retrieved::miss(),
            Err(error) => {
                // Retrieval is fail-open: a store that cannot answer costs the
                // model its fragment, never the request.
                tracing::warn!(%error, "ref store read failed; reporting a miss");
                Retrieved::miss()
            }
        };

        self.record_retrieval(identity, handle, &result, request, surface, time_ms);
        result
    }

    /// Retain and verify fragments before publishing their handles.
    ///
    /// Returns false when any fragment cannot be retained or verified. Callers
    /// must then discard the handle-bearing output and return the original.
    pub fn retain_fragments(
        &self,
        identity: &Identity,
        refs: &RefCollector,
        request_id: &str,
        options: &EngineOptions,
    ) -> bool {
        if refs.is_empty() {
            return true;
        }
        let pairs: Vec<_> = refs
            .pairs()
            .iter()
            .map(|(id, fragment)| (*id, fragment.clone()))
            .collect();
        let meta = PutMeta::new(Kind::Text)
            .with_ttl(options.ref_ttl_seconds)
            // Saturating, as `with_ttl` already is. `validate()` accepts any
            // positive TTL, including one a per-request patch supplies, so the
            // product overflows on a large one: a debug build panics inside
            // compression, and a release build wraps to zero and advertises a
            // handle that is already dead.
            .with_max_lifetime(
                options
                    .ref_ttl_seconds
                    .saturating_mul(options.ref_max_lifetime_multiplier as u64),
            )
            .with_request_id(request_id.to_string());
        if let Err(error) = self.refs.put_many(&identity.tenant, &pairs, &meta) {
            tracing::warn!(%error, "could not retain elided fragments");
            return false;
        }
        // A store can accept a batch and immediately evict part of it to obey
        // its byte cap. Never ship a handle that already misses at this point.
        //
        // Length, not content. The handle *is* a hash of the fragment, so a hit
        // under it is that fragment by construction; what a byte cap can do
        // that content addressing cannot rule out is drop or truncate the row.
        // Comparing the bytes as well would mean an equality pass over every
        // fragment on every request — up to `max_ref_bytes` each, now a
        // mebibyte — for a difference the id already excludes.
        for (id, fragment) in &pairs {
            match self.refs.verify(&identity.tenant, id) {
                Ok(Lookup::Hit(record)) if record.fragment.len() == fragment.len() => {}
                other => {
                    tracing::warn!(handle = %id, ?other, "newly emitted fragment is not retrievable");
                    return false;
                }
            }
        }
        true
    }

    /// Push out the retention window of handles sitting in content we declined
    /// to recompress.
    ///
    /// A handle cannot be *created* in protected content, because protection
    /// short-circuits compression. It drifts there: a tool result compressed on
    /// an early turn keeps its marker verbatim, and later the frozen prefix
    /// grows past it or the model quotes the marker into its own turn. In the
    /// re-compression alone refreshed retention, the fragment would be on a
    /// fixed clock from that moment and the marker would outlive it — leaving
    /// the model invited to ask for something already gone.
    fn refresh_protected(&self, identity: &Identity, stats: &CompressStats) {
        if stats.protected_refs.is_empty() {
            return;
        }
        let ids: Vec<_> = stats.protected_refs.clone();
        match self.refs.touch_many(&identity.tenant, &ids) {
            Ok(touched) => tracing::debug!(touched, "refreshed handles found in protected content"),
            Err(error) => tracing::warn!(%error, "could not refresh protected handles"),
        }
    }

    fn record_compression(
        &self,
        identity: &Identity,
        stats: &CompressStats,
        request: &RequestInfo<'_>,
        surface: Surface,
        time_ms: i64,
        engine_ms: f64,
    ) {
        if !self.stats.is_enabled() {
            return;
        }
        let mut event = OptimizationEvent::new(
            time_ms,
            identity.tenant.as_str(),
            request.request_id,
            surface,
            stats.mode,
        );
        event.api_family = request.api_family.map(str::to_string);
        event.model = request.model.map(str::to_string);
        event.provider = request.provider.map(str::to_string);
        event.tokens_in = stats.tokens_in;
        event.tokens_out = stats.tokens_out;
        event.chars_in = stats.chars_in;
        event.chars_out = stats.chars_out;
        event.messages_total = stats.compressed_messages + stats.skipped_messages;
        event.messages_compressed = stats.compressed_messages;
        event.messages_skipped = stats.skipped_messages;
        event.refs_emitted = stats.refs_emitted;
        event.ref_bytes_emitted = stats.ref_bytes_emitted;
        event.content_types = stats.by_type.clone();
        event.protected_by_reason = stats.protected_by_reason.clone();
        event.skipped_by_reason = stats.skipped_by_reason.clone();
        event.skipped_tokens_by_reason = stats.skipped_tokens_by_reason.clone();
        event.frozen_messages = stats.frozen_messages;
        event.engine_ms = engine_ms;
        // Name the zero. A request that removed nothing is not an error, but an
        // operator who is only shown the zero cannot tell a client that pinned
        // its whole prompt from a payload the engine had nothing for.
        let outcome = stats.outcome();
        if outcome == Outcome::PrefixFrozen {
            tracing::warn!(
                messages = stats.skipped_messages,
                chars = stats.chars_in,
                "nothing was compressible: the client pinned the whole prompt with cache_control"
            );
        }
        event.outcome = Some(outcome.as_str().to_string());
        self.stats.send(Event::Optimization(event));

        if !stats.protected_refs.is_empty() {
            self.stats.send(Event::ProtectedRefs(ProtectedRefEvent {
                time_ms,
                tenant: identity.tenant.as_str().to_string(),
                request_id: Some(request.request_id.to_string()),
                refs: stats.protected_refs.iter().map(|r| r.to_string()).collect(),
                reasons: stats.protected_ref_reasons.iter().cloned().collect(),
                messages: stats.protected_ref_messages,
                touched: stats.protected_refs.len() as u32,
            }));
        }
    }

    /// File a retrieval the budget refused before the store was read.
    ///
    /// The three refusal sites — the buffered path, the SSE path and the
    /// websocket relay — all answered the model and logged a warning, and left
    /// nothing in the database. `retrieval_events` recorded what came back, so
    /// the one metric an operator reads to tune
    /// `proxy.retrieve_max_tokens_per_request` was computed entirely over the
    /// asks that got past it. On a measured sweep that printed
    /// `hits 7 (100.0%)` for a connection that had been refused 11 times.
    ///
    /// Filed with no sizes, because nothing was read. `peek` is still not
    /// called here: pricing a refusal would mean reading the fragment to
    /// measure what was withheld, which is the read this whole gate exists to
    /// avoid paying for.
    pub fn record_refusal(
        &self,
        identity: &Identity,
        handle: &str,
        request: &RequestInfo<'_>,
        surface: Surface,
    ) {
        if !self.stats.is_enabled() {
            return;
        }
        self.stats.send(Event::Retrieval(RetrievalEvent {
            time_ms: now_ms(),
            tenant: identity.tenant.as_str().to_string(),
            request_id: Some(request.request_id.to_string()).filter(|r| !r.is_empty()),
            surface,
            reference: handle.to_string(),
            found: false,
            expired: false,
            ranged: false,
            fragment_bytes: 0,
            fragment_chars: 0,
            fragment_tokens: 0,
            returned_chars: 0,
            returned_tokens: 0,
            withheld: true,
        }));
    }

    fn record_retrieval(
        &self,
        identity: &Identity,
        handle: &str,
        result: &Retrieved,
        request: &RequestInfo<'_>,
        surface: Surface,
        time_ms: i64,
    ) {
        if !self.stats.is_enabled() {
            return;
        }
        let returned = result.content.as_deref().unwrap_or("");
        self.stats.send(Event::Retrieval(RetrievalEvent {
            time_ms,
            tenant: identity.tenant.as_str().to_string(),
            request_id: Some(request.request_id.to_string()).filter(|r| !r.is_empty()),
            surface,
            reference: handle.to_string(),
            found: result.found,
            expired: result.expired,
            ranged: result.ranged,
            fragment_bytes: returned.len() as u64,
            fragment_chars: result.total_chars as u64,
            // What the model is actually re-charged for is what came back, not
            // what the fragment holds, so a narrowed retrieval is priced as the
            // cheaper thing it is.
            fragment_tokens: kompressor_engine::estimate_tokens(returned, ""),
            returned_chars: returned.chars().count() as u64,
            returned_tokens: kompressor_engine::estimate_tokens(returned, ""),
            withheld: false,
        }));
    }
}

/// Record fragments the engine refused to retain, so a 0.00% that came from
/// `max_ref_bytes` is distinguishable from one that came from having nothing to
/// compress.
///
/// Both read the same in a stats report otherwise, and `--explain` is the only
/// place that currently tells them apart — which is no help to an operator
/// looking at a dashboard. A refusal the compressor handled by keeping its
/// content retracts itself and never reaches here; what is left is a marker
/// that could not be honoured, which is why the payload came back whole.
fn note_oversized_refusals(stats: &mut CompressStats, refs: &RefCollector, request_id: &str) {
    let refused = refs.oversized_refusals();
    if refused == 0 {
        return;
    }
    *stats
        .skipped_by_reason
        .entry("refs_oversized".to_string())
        .or_insert(0) += refused as u32;
    tracing::warn!(
        request_id,
        refused,
        max_ref_bytes = refs.max_ref_bytes(),
        "declined to compress: an elision was larger than `max_ref_bytes` and could not be \
         retained, so the payload was returned uncompressed; raise `[engine] max_ref_bytes` \
         or lower the amount elided"
    );
}

fn discard_compression_stats(stats: &mut CompressStats) {
    stats.tokens_out = stats.tokens_in;
    stats.chars_out = stats.chars_in;
    stats.skipped_messages = stats
        .skipped_messages
        .saturating_add(stats.compressed_messages);
    stats.compressed_messages = 0;
    stats.by_type.clear();
    stats.refs_emitted = 0;
    stats.ref_bytes_emitted = 0;
    stats.ratio = 1.0;
}

/// Build the ref store a config asks for.
pub fn build_ref_store(config: &Config) -> Result<Arc<dyn RefStore>, crate::error::CoreError> {
    use crate::config::RefsBackend;
    use kompressor_store::cache::CachedRefStore;
    use kompressor_store::memory::MemoryRefStore;
    use kompressor_store::sqlite::{SqliteOptions, SqliteRefStore};

    match config.refs.backend {
        RefsBackend::Memory => Ok(Arc::new(
            MemoryRefStore::new().with_max_bytes(config.refs.max_bytes),
        )),
        RefsBackend::Sqlite => {
            if let Some(parent) = config.refs.path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let store = SqliteRefStore::open_with(
                &config.refs.path,
                SqliteOptions {
                    max_bytes: Some(config.refs.max_bytes),
                    ..Default::default()
                },
            )?;
            // A retrieval can arrive milliseconds after the compression that
            // minted the handle, so the most recent fragments are served from
            // memory rather than racing the write behind them.
            Ok(Arc::new(
                CachedRefStore::new(store).with_capacity(config.refs.recent_cache_entries),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_store::memory::MemoryRefStore;
    use kompressor_store::types::Tenant;

    fn core() -> AppCore {
        let config = Config::default();
        AppCore::new(
            config,
            Arc::new(MemoryRefStore::new()),
            kompressor_stats::writer::null_sink(),
        )
    }

    fn log_payload(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i}",
                    i % 60
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A TTL `validate()` accepts must not overflow the lifetime it implies.
    ///
    /// `EngineOptions::validate` admits any positive `u64`, including one a
    /// per-request patch supplies, and retention multiplied it by
    /// `ref_max_lifetime_multiplier` unchecked. A debug build panicked inside
    /// compression — turning a working provider call into our 500, the one
    /// thing the proxy's fail-open rule forbids — and a release build wrapped
    /// to zero, retaining fragments that were expired the moment they were
    /// advertised.
    #[test]
    fn a_huge_ttl_does_not_overflow_the_lifetime_it_implies() {
        let core = core();
        let identity = Identity::anonymous();
        let mut options = core.options_for(&identity, None);
        options.ref_ttl_seconds = 1u64 << 61;
        assert!(
            options.validate().is_ok(),
            "the fixture must be a TTL validation accepts, or this proves nothing"
        );

        let mut messages = vec![Message::text("user", log_payload(400))];
        let outcome = core.compress(
            &identity,
            &mut messages,
            &RequestInfo {
                request_id: "req-ttl",
                ..Default::default()
            },
            &options,
            0,
            "",
            Surface::Api,
            PrefixCache::Warm,
            &[],
            &[],
            1000,
        );
        assert!(
            outcome.stats.refs_emitted > 0,
            "the fragment must be retained, not dropped or wrapped to dead"
        );

        // And it is still there: a wrapped lifetime of zero made the handle
        // expire immediately, which reads as retention having succeeded.
        let body = match &messages[0].content {
            kompressor_engine::messages::MessageContent::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        let handle = body
            .split("ref:")
            .nth(1)
            .map(|rest| {
                rest.chars()
                    .take_while(|c| *c != ']' && *c != ' ')
                    .collect::<String>()
            })
            .expect("a marker carries a handle");
        let got = core.retrieve(
            &identity,
            &handle,
            None,
            &RequestInfo {
                request_id: "req-ttl",
                ..Default::default()
            },
            Surface::Api,
        );
        assert!(
            got.found,
            "the fragment must outlive the request that minted it"
        );
    }

    #[test]
    fn a_compression_retains_what_it_elided_and_hands_it_back() {
        let core = core();
        let identity = Identity::anonymous();
        let options = core.options_for(&identity, None);
        let mut messages = vec![Message::text("user", log_payload(400))];
        let outcome = core.compress(
            &identity,
            &mut messages,
            &RequestInfo {
                request_id: "req-1",
                ..Default::default()
            },
            &options,
            0,
            "",
            Surface::Api,
            PrefixCache::Warm,
            &[],
            &[],
            1000,
        );
        assert!(
            outcome.stats.refs_emitted > 0,
            "recoverable retains fragments"
        );

        let body = match &messages[0].content {
            kompressor_engine::messages::MessageContent::Text(t) => t.clone(),
            _ => panic!(),
        };
        let handle = body
            .split("ref:")
            .nth(1)
            .map(|rest| {
                rest.chars()
                    .take_while(|c| *c != ']' && *c != ' ')
                    .collect::<String>()
            })
            .expect("a marker carries a handle");

        let got = core.retrieve(
            &identity,
            &handle,
            None,
            &RequestInfo {
                request_id: "req-1",
                ..Default::default()
            },
            Surface::Api,
        );
        assert!(got.found, "the handle the model was given resolves");
        assert!(!got.content.unwrap().is_empty());
    }

    #[test]
    fn recoverable_mode_does_not_ship_handles_evicted_on_write() {
        let config = Config::default();
        let core = AppCore::new(
            config,
            Arc::new(MemoryRefStore::new().with_max_bytes(1)),
            kompressor_stats::writer::null_sink(),
        );
        let input = log_payload(400);
        let options = core.options_for(&Identity::anonymous(), None);
        let (output, outcome) = core.compress_raw(
            &Identity::anonymous(),
            &input,
            None,
            &RequestInfo::default(),
            &options,
            Surface::Cli,
        );
        assert_eq!(output, input);
        assert_eq!(outcome.stats.refs_emitted, 0);
        assert_eq!(outcome.stats.tokens_out, outcome.stats.tokens_in);
    }

    /// A computed compression that is never published touches nothing.
    ///
    /// This is what lets the proxy race compression against a deadline. A
    /// `spawn_blocking` task cannot be cancelled, so the loser runs to
    /// completion; the guarantee has to be that its result is inert until
    /// someone publishes it. Before the split it was not: the abandoned task
    /// retained fragments, filed savings for an output never forwarded, and
    /// moved inline-promotion state.
    #[test]
    fn a_computed_compression_is_inert_until_it_is_published() {
        let (sink, captured) = kompressor_stats::writer::capturing_sink(64);
        let store = Arc::new(MemoryRefStore::new());
        let core = AppCore::new(Config::default(), store.clone(), sink);
        let identity = Identity::anonymous();
        let options = core.options_for(&identity, None);
        let mut messages = vec![Message::text("user", log_payload(400))];

        let computed = core.compress_only(
            &mut messages,
            &options,
            0,
            "",
            PrefixCache::Warm,
            &[],
            &[],
            1000,
            0,
            None,
        );
        assert!(
            computed.stats.tokens_out < computed.stats.tokens_in,
            "the work really happened"
        );
        assert_eq!(
            store.stats().expect("store stats").live_refs,
            0,
            "no fragment may be retained before publication"
        );
        assert!(
            captured.drain().is_empty(),
            "no event may be filed before publication"
        );

        let outcome = core.publish(
            &identity,
            &mut messages,
            computed,
            &RequestInfo::default(),
            &options,
            Surface::Proxy,
        );
        assert!(outcome.stats.tokens_out < outcome.stats.tokens_in);
        assert!(
            store.stats().expect("store stats").live_refs > 0,
            "publication retains what was elided"
        );
        assert_eq!(
            captured.drain().len(),
            1,
            "publication files exactly one event"
        );
    }

    #[test]
    fn recoverable_mode_declines_an_elision_over_the_fragment_limit() {
        let core = core();
        let input = log_payload(400);
        let mut options = core.options_for(&Identity::anonymous(), None);
        options.max_ref_bytes = 8;
        let (output, _) = core.compress_raw(
            &Identity::anonymous(),
            &input,
            None,
            &RequestInfo::default(),
            &options,
            Surface::Cli,
        );
        assert_eq!(output, input);
    }

    /// A payload declined for the fragment limit says so in its stats.
    ///
    /// Without this, the row is indistinguishable from one the engine simply
    /// found nothing to compress in: same 0.00%, same empty `by_type`. The
    /// operator sees a dashboard that reads "nothing to do" when the truth is
    /// "gave up", and the one surface that told them apart was `--explain`,
    /// which nobody runs against production traffic.
    #[test]
    fn a_payload_declined_for_the_fragment_limit_is_visible_in_stats() {
        let core = core();
        let input = log_payload(400);
        let mut options = core.options_for(&Identity::anonymous(), None);
        options.max_ref_bytes = 8;
        let (output, outcome) = core.compress_raw(
            &Identity::anonymous(),
            &input,
            None,
            &RequestInfo::default(),
            &options,
            Surface::Cli,
        );
        assert_eq!(output, input, "it still declines");
        assert!(
            outcome
                .stats
                .skipped_by_reason
                .contains_key("refs_oversized"),
            "the reason must be recorded, got {:?}",
            outcome.stats.skipped_by_reason
        );

        // …and a payload that simply had nothing to compress does not claim it.
        let prose = "Some ordinary prose that no compressor claims. ".repeat(30);
        let options = core.options_for(&Identity::anonymous(), None);
        let (_, outcome) = core.compress_raw(
            &Identity::anonymous(),
            &prose,
            None,
            &RequestInfo::default(),
            &options,
            Surface::Cli,
        );
        assert!(
            !outcome
                .stats
                .skipped_by_reason
                .contains_key("refs_oversized"),
            "nothing was refused here, got {:?}",
            outcome.stats.skipped_by_reason
        );
    }

    #[test]
    fn a_handle_does_not_resolve_for_another_tenant() {
        let core = core();
        let a = Identity {
            tenant: Tenant::new("team-a"),
            mode: None,
            trusted: false,
        };
        let b = Identity {
            tenant: Tenant::new("team-b"),
            ..a.clone()
        };
        let options = core.options_for(&a, None);
        let mut messages = vec![Message::text("user", log_payload(400))];
        core.compress(
            &a,
            &mut messages,
            &RequestInfo::default(),
            &options,
            0,
            "",
            Surface::Api,
            PrefixCache::Warm,
            &[],
            &[],
            1000,
        );
        let body = match &messages[0].content {
            kompressor_engine::messages::MessageContent::Text(t) => t.clone(),
            _ => panic!(),
        };
        let handle: String = body
            .split("ref:")
            .nth(1)
            .map(|rest| {
                rest.chars()
                    .take_while(|c| *c != ']' && *c != ' ')
                    .collect()
            })
            .unwrap();

        assert!(
            core.retrieve(&a, &handle, None, &RequestInfo::default(), Surface::Api)
                .found
        );
        assert!(
            !core
                .retrieve(&b, &handle, None, &RequestInfo::default(), Surface::Api)
                .found,
            "the tenant is part of the key, not a filter applied afterwards"
        );
    }

    #[test]
    fn a_malformed_handle_is_a_miss_rather_than_an_error() {
        let core = core();
        let got = core.retrieve(
            &Identity::anonymous(),
            "not-a-handle",
            None,
            &RequestInfo::default(),
            Surface::Api,
        );
        assert!(!got.found);
        assert!(got.miss_hint().contains("Re-run"));
    }

    #[test]
    fn a_tenant_mode_overrides_the_deployment_default_and_a_request_overrides_both() {
        let core = core();
        let identity = Identity {
            tenant: Tenant::new("team-a"),
            mode: Some(Mode::Aggressive),
            trusted: false,
        };
        assert_eq!(core.options_for(&identity, None).mode, Mode::Aggressive);

        let patch = EngineOptionsPatch {
            mode: Some(Mode::Lossless),
            ..Default::default()
        };
        assert_eq!(
            core.options_for(&identity, Some(&patch)).mode,
            Mode::Lossless
        );
    }

    #[test]
    fn a_raw_payload_is_routed_as_a_tool_result() {
        let core = core();
        let identity = Identity::anonymous();
        let options = core.options_for(&identity, None);
        let (out, outcome) = core.compress_raw(
            &identity,
            &log_payload(400),
            None,
            &RequestInfo::default(),
            &options,
            Surface::Cli,
        );
        assert_eq!(outcome.stats.compressed_messages, 1);
        assert!(out.chars().count() < log_payload(400).chars().count());
        assert!(!outcome.stats.by_type.is_empty());
    }

    #[test]
    fn a_ranged_retrieval_returns_less_than_the_whole_fragment() {
        let core = core();
        let identity = Identity::anonymous();
        let options = core.options_for(&identity, None);
        let mut messages = vec![Message::text("user", log_payload(400))];
        core.compress(
            &identity,
            &mut messages,
            &RequestInfo::default(),
            &options,
            0,
            "",
            Surface::Api,
            PrefixCache::Warm,
            &[],
            &[],
            1000,
        );
        let body = match &messages[0].content {
            kompressor_engine::messages::MessageContent::Text(t) => t.clone(),
            _ => panic!(),
        };
        let handle: String = body
            .split("ref:")
            .nth(1)
            .map(|rest| {
                rest.chars()
                    .take_while(|c| *c != ']' && *c != ' ')
                    .collect()
            })
            .unwrap();

        let whole = core.retrieve(
            &identity,
            &handle,
            None,
            &RequestInfo::default(),
            Surface::Api,
        );
        let window = core.retrieve(
            &identity,
            &handle,
            Some(&RetrieveRange::Lines { start: 1, end: 3 }),
            &RequestInfo::default(),
            Surface::Api,
        );
        assert!(window.ranged);
        assert!(
            window.content.unwrap().len() < whole.content.unwrap().len(),
            "a range is what keeps a retrieval from costing more than it saved"
        );
    }
}
