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
    /// `config` as each provider named in `proxy.providers` sees it, merged
    /// once here rather than on every request.
    providers: std::collections::BTreeMap<String, Config>,
    auth: Authenticator,
    refs: Arc<dyn RefStore>,
    stats: StatsSink,
    /// Permits for engine work, shared by every surface in the process.
    ///
    /// Sized [`crate::config::ServerConfig::inflight_limit`]. It lives here
    /// rather than on one surface's state because the HTTP API was the only
    /// surface that ever acquired one: the proxy and MCP spawned blocking work
    /// directly, so a burst of large histories on either could queue and run
    /// without limit — and a proxy timeout does not cancel the task it gave up
    /// on, so abandoned work kept running and kept its copy of the messages.
    engine_slots: Arc<tokio::sync::Semaphore>,
    /// What the last sweep of expired fragments did. In memory, as the
    /// integrity checks are: it is about this process.
    refs_sweep: std::sync::Mutex<Option<RefsSweep>>,
    /// The listeners this process is bound to and the requests it carries.
    runtime: Arc<crate::runtime::Runtime>,
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

/// A computed result whose fragments have been through the store.
///
/// The middle of three stages — compute, retain, file — and the only one that
/// does storage I/O. [`AppCore::retain`] produces it on a blocking thread;
/// [`AppCore::file`] consumes it on whatever thread decided to accept the
/// result, and does nothing there that can block.
///
/// Dropping one without filing leaves fragments in the store that no forwarded
/// request names. They are content-addressed, so a later request that elides
/// the same text re-uses the row rather than duplicating it, and otherwise
/// they expire on the ordinary idle window. No saving is recorded and no
/// promotion moves, which is what made abandoning a result safe before the
/// split and still does.
pub struct Retained {
    computed: Computed,
    /// Every fragment was retained and verified. When false, filing reverts
    /// the messages to the originals and records no saving.
    retained: bool,
}

/// Where a compression came from, for attribution.
#[derive(Clone, Debug, Default)]
pub struct RequestInfo<'a> {
    pub request_id: &'a str,
    pub api_family: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
}

/// Who a hook-mode result belongs to, beyond what [`RequestInfo`] carries.
///
/// A hook is handed one tool result at a time with no message array around
/// it, so the tool and the agent's own conversation id are the only way to
/// group its rows afterwards. Every other surface passes the default.
#[derive(Clone, Copy, Debug, Default)]
pub struct Attribution<'a> {
    /// The agent's tool name: `Shell`, `MCP:<tool>`, `Read`.
    pub tool: Option<&'a str>,
    /// The agent's conversation id.
    pub conversation: Option<&'a str>,
    /// Measure only: retain nothing, and file the row as `mode = 'off'` so
    /// the would-be removal is never read as a saving.
    pub shadow: bool,
    /// The most the agent would have put in front of the model without us.
    ///
    /// Cursor writes any tool result over 40,000 bytes to a file and hands
    /// the model a reference instead, so for such a result the uncompressed
    /// baseline was not the output: it was nothing inline. Counting the whole
    /// output as removed claimed ~78k tokens for a `seq 1 20000` the model
    /// would never have received (measured on the real CLI, 2026-11-25). For
    /// a payload over this limit the row's `tokens_in` is set to its
    /// `tokens_out` — no removal is claimed — while `chars_in` keeps the real
    /// size, which is how a report tells such a result apart.
    pub inline_limit_bytes: Option<usize>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One sweep of expired fragments out of the ref store
/// (`refs.purge_interval`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RefsSweep {
    /// When it ran, and for how long.
    pub at_ms: i64,
    pub took_ms: u64,
    /// Fragments it removed. Absent when it failed.
    pub removed: Option<u64>,
    /// Bytes of free pages it then handed back to the disk
    /// (`RefStore::reclaim`). Absent when it failed.
    pub reclaimed_bytes: Option<u64>,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

impl AppCore {
    pub fn new(config: Config, refs: Arc<dyn RefStore>, stats: StatsSink) -> AppCore {
        let stats = stats.with_context(crate::analysis_context::context(&config));
        let auth = Authenticator::new(config.auth.clone());
        let providers = config
            .proxy
            .providers
            .keys()
            .filter_map(|name| Some((name.clone(), config.for_provider(name)?)))
            .collect();
        AppCore {
            providers,
            auth,
            engine_slots: Arc::new(tokio::sync::Semaphore::new(config.server.inflight_limit())),
            refs,
            stats,
            config,
            refs_sweep: std::sync::Mutex::new(None),
            runtime: Arc::default(),
        }
    }

    /// The process's record of its listeners and of the requests under way.
    pub fn runtime(&self) -> &Arc<crate::runtime::Runtime> {
        &self.runtime
    }

    /// Put on record what a sweep of expired fragments did, for the
    /// Maintenance page. Whoever runs the sweep calls this: `serve` does.
    pub fn note_refs_sweep(&self, sweep: RefsSweep) {
        *self
            .refs_sweep
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sweep);
    }

    /// The last sweep of expired fragments in this process, if there was one.
    pub fn refs_sweep(&self) -> Option<RefsSweep> {
        self.refs_sweep
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The permits every surface acquires before running the engine.
    ///
    /// Hold the permit inside the blocking closure, not around the `await`
    /// on it: a caller that stops waiting does not stop the work, and the
    /// permit has to stay taken until the work actually ends.
    pub fn engine_slots(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.engine_slots
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The configuration one provider's traffic runs under: `proxy.providers`
    /// applied when it names `provider`, the deployment's otherwise.
    pub fn config_for(&self, provider: Option<&str>) -> &Config {
        provider
            .and_then(|name| self.providers.get(name))
            .unwrap_or(&self.config)
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
        self.options_for_provider(identity, None, patch)
    }

    /// As [`options_for`](Self::options_for), starting from the engine options
    /// of `provider` (see [`config_for`](Self::config_for)). The tenant's mode
    /// and the request's patch still apply on top.
    pub fn options_for_provider(
        &self,
        identity: &Identity,
        provider: Option<&str>,
        patch: Option<&EngineOptionsPatch>,
    ) -> EngineOptions {
        let mut options = self.config_for(provider).engine.clone();
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
            read_ledger: None,
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
        // What this conversation has decided about its file reads, and
        // whether this run may decide more. `None` is the read lifecycle as it
        // always was; see `kompressor_engine::messages::ReadLedger`.
        read_ledger: Option<kompressor_engine::messages::ReadLedger<'_>>,
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
            read_ledger,
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
    ///
    /// The composition of [`AppCore::retain`] and [`AppCore::file`], for a
    /// caller already on a blocking thread that has nothing to decide between
    /// the two. The proxy calls them separately so the store I/O falls inside
    /// its deadline and the filing outside it.
    pub fn publish(
        &self,
        identity: &Identity,
        messages: &mut [Message],
        computed: Computed,
        request: &RequestInfo<'_>,
        options: &EngineOptions,
        surface: Surface,
    ) -> CompressOutcome {
        let retained = self.retain(identity, computed, request.request_id, options);
        self.file(identity, messages, retained, request, options, surface)
    }

    /// Put a computed result's fragments in the store, and refresh the handles
    /// it found in protected content.
    ///
    /// Blocking: a SQLite write behind a mutex and a 5-second busy timeout.
    /// Run it where blocking is allowed — on the proxy, inside the same
    /// `spawn_blocking` task and the same deadline as the computation, so a
    /// stalled store falls the request open instead of stalling the runtime
    /// worker every other stream on it shares.
    pub fn retain(
        &self,
        identity: &Identity,
        computed: Computed,
        request_id: &str,
        options: &EngineOptions,
    ) -> Retained {
        let retained = self.retain_fragments(identity, &computed.refs, request_id, options);
        self.refresh_protected(identity, &computed.stats);
        Retained { computed, retained }
    }

    /// Accept a retained result: revert it if retention failed, then record it.
    ///
    /// Nothing here blocks — the event goes to the statistics writer's bounded
    /// queue, which drops rather than waits — so it is safe on an async worker,
    /// and it is the commit point: a result never filed has recorded nothing.
    pub fn file(
        &self,
        identity: &Identity,
        messages: &mut [Message],
        retained: Retained,
        request: &RequestInfo<'_>,
        options: &EngineOptions,
        surface: Surface,
    ) -> CompressOutcome {
        let Retained {
            computed:
                Computed {
                    mut stats,
                    refs,
                    original,
                    time_ms,
                    engine_ms,
                },
            retained,
        } = retained;

        // On this side of the split rather than in `compress_only`: the note
        // is part of what gets filed, so a computed result the caller abandons
        // should not produce one.
        note_oversized_refusals(&mut stats, &refs, request.request_id);

        if !retained && let Some(original) = original {
            messages.clone_from_slice(&original);
            discard_compression_stats(&mut stats);
        }
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
        self.compress_raw_attributed(
            identity,
            content,
            forced,
            request,
            options,
            surface,
            &Attribution::default(),
        )
    }

    /// [`compress_raw`](Self::compress_raw), with the row attributed to an
    /// agent tool and conversation, or — with `attribution.shadow` — a dry
    /// run that retains nothing and returns the input unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn compress_raw_attributed(
        &self,
        identity: &Identity,
        content: &str,
        forced: Option<ForcedType>,
        request: &RequestInfo<'_>,
        options: &EngineOptions,
        surface: Surface,
        attribution: &Attribution<'_>,
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

        if let Some(limit) = attribution.inline_limit_bytes
            && content.len() > limit
        {
            stats.tokens_in = stats.tokens_out;
            stats.ratio = 1.0;
        }
        note_oversized_refusals(&mut stats, &refs, request.request_id);
        if attribution.shadow {
            // Nothing is retained, so nothing is published: the row says what
            // the engine would have removed, and the caller gets its input.
            self.record_compression_attributed(
                identity,
                &stats,
                request,
                surface,
                time_ms,
                engine_ms,
                attribution,
            );
            return (
                content.to_string(),
                CompressOutcome {
                    stats,
                    mode: options.mode,
                },
            );
        }
        let retained = self.retain_fragments(identity, &refs, request.request_id, options);
        if !retained && options.mode == Mode::Recoverable {
            discard_compression_stats(&mut stats);
        }
        self.record_compression_attributed(
            identity,
            &stats,
            request,
            surface,
            time_ms,
            engine_ms,
            attribution,
        );

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
        // A typed surface either deserialized the range exactly or refused
        // the call; there is no salvage on those paths to report.
        let status = range.map(|_| crate::retrieve::RangeParse::Exact);
        self.retrieve_as(identity, handle, range, status, request, surface)
    }

    /// [`AppCore::retrieve`], saying how the range was read, so the retrieval
    /// event can tell a failed narrowing from a whole fragment asked for.
    /// `status` is `None` when the caller sent no range at all.
    pub fn retrieve_as(
        &self,
        identity: &Identity,
        handle: &str,
        range: Option<&RetrieveRange>,
        status: Option<crate::retrieve::RangeParse>,
        request: &RequestInfo<'_>,
        surface: Surface,
    ) -> Retrieved {
        let status = status.map(crate::retrieve::RangeParse::as_str);
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
                status,
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

        self.record_retrieval(identity, handle, &result, request, surface, time_ms, status);
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
        let base = PutMeta::new(Kind::Text)
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
        // One batch per kind: the store records a kind per `PutMeta`, and a
        // request's fragments come out of payloads of different types.
        let mut by_kind: Vec<(Kind, Vec<_>)> = Vec::new();
        for pair in &pairs {
            let kind = refs.kind_of(&pair.0);
            match by_kind.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, group)) => group.push(pair.clone()),
                None => by_kind.push((kind, vec![pair.clone()])),
            }
        }
        for (kind, group) in &by_kind {
            let meta = PutMeta {
                kind: *kind,
                ..base.clone()
            };
            if let Err(error) = self.refs.put_many(&identity.tenant, group, &meta) {
                tracing::warn!(%error, "could not retain elided fragments");
                return false;
            }
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
        self.record_elisions(identity, refs, request_id);
        true
    }

    /// File the handles about to be shipped, with their kind and size: the
    /// denominator of a fetch probability, which `retrieval_events` alone
    /// cannot give (it holds only the handles that were asked for). The
    /// writer keeps the first request to ship each handle.
    fn record_elisions(&self, identity: &Identity, refs: &RefCollector, request_id: &str) {
        if !self.stats.is_enabled() {
            return;
        }
        let elided = refs
            .pairs()
            .iter()
            .map(|(id, fragment)| kompressor_stats::events::Elided {
                reference: id.to_string(),
                kind: refs.kind_of(id).as_str().to_string(),
                bytes: fragment.len() as u64,
            })
            .collect();
        self.stats
            .send(Event::Elisions(kompressor_stats::events::ElisionsEvent {
                time_ms: now_ms(),
                tenant: identity.tenant.as_str().to_string(),
                request_id: request_id.to_string(),
                refs: elided,
            }));
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
        self.record_compression_attributed(
            identity,
            stats,
            request,
            surface,
            time_ms,
            engine_ms,
            &Attribution::default(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_compression_attributed(
        &self,
        identity: &Identity,
        stats: &CompressStats,
        request: &RequestInfo<'_>,
        surface: Surface,
        time_ms: i64,
        engine_ms: f64,
        attribution: &Attribution<'_>,
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
        event.tool = attribution.tool.map(str::to_string);
        event.conversation = attribution.conversation.map(str::to_string);
        event.shadow = attribution.shadow;
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
            context: None,
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
            range_status: None,
        }));
    }

    #[allow(clippy::too_many_arguments)]
    fn record_retrieval(
        &self,
        identity: &Identity,
        handle: &str,
        result: &Retrieved,
        request: &RequestInfo<'_>,
        surface: Surface,
        time_ms: i64,
        range_status: Option<&str>,
    ) {
        if !self.stats.is_enabled() {
            return;
        }
        let returned = result.content.as_deref().unwrap_or("");
        self.stats.send(Event::Retrieval(RetrievalEvent {
            context: None,
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
            range_status: range_status.map(str::to_string),
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
        RefsBackend::Redis => {
            use kompressor_store::redis::{RedisOptions, RedisRefStore};
            let mut options = RedisOptions::new(&config.refs.redis_url).map_err(|e| {
                crate::error::CoreError::Store(kompressor_store::StoreError::Redis(e))
            })?;
            options.prefix = config.refs.redis_prefix.clone();
            options.timeout = config.refs.redis_timeout;
            options.pool = config.refs.redis_pool;
            let store = RedisRefStore::open(options)?;
            // Shared with other proxies, so a retrieval racing its own write
            // is served here rather than across the network.
            Ok(Arc::new(
                CachedRefStore::new(store)
                    .with_capacity(config.refs.recent_cache_entries)
                    .with_max_bytes(config.refs.recent_cache_bytes),
            ))
        }
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
                CachedRefStore::new(store)
                    .with_capacity(config.refs.recent_cache_entries)
                    .with_max_bytes(config.refs.recent_cache_bytes),
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

    /// The store's `kind` column used to read `text` for every row, because
    /// one `PutMeta` covered the whole request.
    #[test]
    fn retained_fragments_keep_the_kind_they_were_elided_from() {
        let store = Arc::new(MemoryRefStore::new());
        let core = AppCore::new(
            Config::default(),
            store.clone(),
            kompressor_stats::writer::null_sink(),
        );
        let identity = Identity::anonymous();
        let options = core.options_for(&identity, None);
        let mut refs = options.collector();
        refs.set_kind(Kind::Json);
        let json = refs.stash("{\"a\": 1}").unwrap();
        refs.set_kind(Kind::SourceCode);
        let code = refs.stash("fn main() {}").unwrap();
        refs.set_kind(Kind::Json);
        let json2 = refs.stash("{\"b\": 2}").unwrap();

        assert!(core.retain_fragments(&identity, &refs, "req-1", &options));

        for (id, want) in [(json, "json"), (code, "source_code"), (json2, "json")] {
            match store.verify(&identity.tenant, &id).unwrap() {
                Lookup::Hit(record) => assert_eq!(record.kind, want),
                other => panic!("{id} not stored: {other:?}"),
            }
        }
    }

    /// Every shipped handle is filed with its kind and size, which is what
    /// a fetch probability is divided by.
    #[test]
    fn retained_fragments_are_filed_as_elisions_with_their_kind_and_size() {
        let (sink, captured) = kompressor_stats::writer::capturing_sink(16);
        let core = AppCore::new(Config::default(), Arc::new(MemoryRefStore::new()), sink);
        let identity = Identity::anonymous();
        let options = core.options_for(&identity, None);
        let mut refs = options.collector();
        refs.set_kind(Kind::Json);
        let json = refs.stash("{\"a\": 1}").unwrap();
        refs.set_kind(Kind::SourceCode);
        let code = refs.stash("fn main() {}").unwrap();

        assert!(core.retain_fragments(&identity, &refs, "req-7", &options));

        let filed: Vec<_> = captured
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                Event::Elisions(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].request_id, "req-7");
        let shipped: Vec<(String, String, u64)> = filed[0]
            .refs
            .iter()
            .map(|e| (e.reference.clone(), e.kind.clone(), e.bytes))
            .collect();
        assert_eq!(
            shipped,
            [
                (json.to_string(), "json".to_string(), 8),
                (code.to_string(), "source_code".to_string(), 12),
            ]
        );
        assert!(shipped[0].0.starts_with("k_"), "as retrievals name it");
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
        let filed = captured.drain();
        assert_eq!(
            filed
                .iter()
                .filter(|event| matches!(event, Event::Optimization(_)))
                .count(),
            1,
            "publication files exactly one compression"
        );
        assert_eq!(
            filed
                .iter()
                .filter(|event| matches!(event, Event::Elisions(_)))
                .count(),
            1,
            "and the handles it shipped"
        );
        assert_eq!(filed.len(), 2);
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
