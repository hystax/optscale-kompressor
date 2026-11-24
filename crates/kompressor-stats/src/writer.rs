//! The writer: a bounded queue, one thread, one transaction per batch.
//!
//! Two rules shape everything here.
//!
//! **Analytics never touches the request path.** [`StatsSink::send`] takes no
//! lock the request cares about, does no I/O, cannot block and cannot fail. When
//! the queue is full the event is dropped and counted. A dropped row costs a
//! line in a report; a blocked compression costs the request.
//!
//! **One thread owns the connection.** SQLite serialises writers anyway, so a
//! connection per request would buy nothing but lock contention and an fsync
//! per row. Batching into one transaction every [`crate::DEFAULT_FLUSH_INTERVAL`]
//! turns a request-rate insert load into a ten-per-second commit load.
//!
//! There is no tokio here on purpose: the crate has to be callable from the CLI
//! and from sync library code, and a thread blocking on `recv_timeout` is not
//! async work — making it a task would only mean an async runtime is required
//! to record that something happened.

use crate::config::StatsConfig;
use crate::error::StatsError;
use crate::events::{
    Event, OptimizationEvent, ProtectedRefEvent, ProxyRequestEvent, RetrievalEvent,
};
use crate::schema;
use rusqlite::{Connection, params};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What travels down the queue. Control messages share it with events so a
/// flush cannot overtake the events it is meant to flush.
pub(crate) enum Msg {
    /// Append this.
    Event(Box<Event>),
    /// Commit everything queued before this, then acknowledge.
    Flush(mpsc::Sender<Result<(), String>>),
    /// Commit everything queued before this, then run the job on the writer
    /// thread: nothing is written to the database while it runs.
    Exclusive(Box<dyn FnOnce() + Send>),
    /// Commit and stop. Anything queued behind this is discarded, so a caller
    /// with events it cares about flushes before shutting down.
    Shutdown,
}

struct SinkInner {
    tx: SyncSender<Msg>,
    // shared with the writer thread, which counts events lost to a failed commit
    dropped: Arc<AtomicU64>,
    // what the writer thread's last retention pass did
    upkeep: Arc<Mutex<Option<Upkeep>>>,
}

/// A cloneable handle for recording events. Cheap to clone, safe to share.
#[derive(Clone)]
pub struct StatsSink {
    // `None` is the null sink: no thread, no file, no counter worth reading.
    inner: Option<Arc<SinkInner>>,
    context: Option<Arc<crate::context::RunContext>>,
}

impl StatsSink {
    /// Record `event`, or drop it if the queue is full.
    ///
    /// Never blocks and never returns an error, by design; see the module doc.
    /// Use [`StatsSink::dropped`] to find out whether anything was lost.
    pub fn send(&self, mut event: Event) {
        let Some(inner) = &self.inner else {
            return;
        };
        if let Some(context) = &self.context {
            match &mut event {
                Event::Optimization(e) => {
                    e.context.get_or_insert_with(|| (**context).clone());
                }
                Event::Retrieval(e) => {
                    e.context.get_or_insert_with(|| (**context).clone());
                }
                Event::ProxyRequest(e) => {
                    e.context.get_or_insert_with(|| (**context).clone());
                }
                _ => {}
            }
        }
        match inner.tx.try_send(Msg::Event(Box::new(event))) {
            Ok(()) => {}
            // Full: the writer is behind. Disconnected: the writer is gone.
            // Neither is a reason to make the caller wait or care.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn with_context(mut self, context: crate::context::RunContext) -> Self {
        self.context = Some(Arc::new(context));
        self
    }

    /// Events lost since start, to a full or dead queue or to a batch whose
    /// commit failed: `stats_dropped_total`.
    ///
    /// Worth exporting rather than merely logging. A report drawn from a period
    /// with a non-zero drop count is an undercount of everything, and the only
    /// way a reader can know that is if this number is published beside it.
    pub fn dropped(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.dropped.load(Ordering::Relaxed))
    }

    /// Whether this sink records anything at all.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Commit everything sent so far and wait for it to land.
    ///
    /// Unlike [`StatsSink::send`] this one blocks, and may block until the
    /// queue drains: it exists for shutdown, for tests and for a CLI that is
    /// about to read back what it wrote, none of which are on a request path.
    pub fn flush(&self) -> Result<(), StatsError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        inner
            .tx
            .send(Msg::Flush(ack_tx))
            .map_err(|_| StatsError::WriterGone)?;
        ack_rx
            .recv()
            .map_err(|_| StatsError::WriterGone)?
            .map_err(StatsError::Batch)
    }

    /// Run `job` while the writer writes nothing, and return what it returns.
    ///
    /// For work on `stats.db` that has to be the only writer for longer than
    /// a busy timeout — a vacuum, erasing it — done on a connection of the
    /// job's own. What was queued before the call is committed first; events
    /// sent while the job runs wait in the queue and are written after it,
    /// and are dropped and counted, as ever, only if the queue fills.
    ///
    /// Blocks until the job has run. With statistics off there is no writer
    /// to hold, and the job runs where it is called.
    pub fn exclusive<T, F>(&self, job: F) -> Result<T, StatsError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let Some(inner) = &self.inner else {
            return Ok(job());
        };
        let (done_tx, done_rx) = mpsc::channel();
        inner
            .tx
            .send(Msg::Exclusive(Box::new(move || {
                let _ = done_tx.send(job());
            })))
            .map_err(|_| StatsError::WriterGone)?;
        done_rx.recv().map_err(|_| StatsError::WriterGone)
    }

    /// What the last retention pass did, or `None` when none has run: with
    /// statistics off, with `retention_days = 0`, or in a process that only
    /// records.
    pub fn upkeep(&self) -> Option<Upkeep> {
        let inner = self.inner.as_ref()?;
        inner
            .upkeep
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// One retention pass of the writer thread: [`StatsWriter::maintain`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Upkeep {
    /// When it ran, and for how long.
    pub at_ms: i64,
    pub took_ms: u64,
    /// The rows it archived and removed, by table. Absent when it failed, or
    /// with `retention_days` zero.
    pub removed: Option<crate::PruneCounts>,
    /// Requests whose copy of the settings it emptied
    /// (`context_retention_days`). Absent when that is off or it failed.
    pub contexts_trimmed: Option<u64>,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

/// Seconds between retention passes.
pub const UPKEEP_INTERVAL_S: u64 = UPKEEP_INTERVAL.as_secs();

/// A sink that discards everything, for `stats.enabled = false`.
///
/// Deliberately the same type rather than a trait object: disabling stats
/// should cost a branch on an `Option`, not a virtual call at every call site,
/// and callers should not need a feature flag or a generic parameter to say
/// "no analytics".
pub fn null_sink() -> StatsSink {
    StatsSink {
        inner: None,
        context: None,
    }
}

/// A sink that keeps every event where a test can read it.
///
/// Exists because the proxy's accounting is only checkable from outside: the
/// question "did this request record a row, and does the row say what actually
/// happened" cannot be answered from the proxy's own return values. Nothing
/// drains the returned channel, so give it room for the events the test expects.
pub fn capturing_sink(capacity: usize) -> (StatsSink, CapturedEvents) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    (
        StatsSink {
            context: None,
            inner: Some(Arc::new(SinkInner {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
                upkeep: Arc::default(),
            })),
        },
        CapturedEvents { rx },
    )
}

/// The receiving end of a [`capturing_sink`].
pub struct CapturedEvents {
    rx: Receiver<Msg>,
}

impl CapturedEvents {
    /// Every event recorded so far, in order.
    pub fn drain(&self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            if let Msg::Event(event) = msg {
                out.push(*event);
            }
        }
        out
    }
}

/// A sink whose queue nothing drains, for testing overflow behaviour.
///
/// The returned receiver must be kept alive, or sends fail as disconnected
/// rather than as full.
#[cfg(test)]
pub(crate) fn detached_sink(capacity: usize) -> (StatsSink, Receiver<Msg>) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    (
        StatsSink {
            context: None,
            inner: Some(Arc::new(SinkInner {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
                upkeep: Arc::default(),
            })),
        },
        rx,
    )
}

/// Owns the database connection and writes batches to it.
///
/// Usable directly for synchronous work — a CLI importing a file, a test — as
/// well as being what the background thread runs.
pub struct StatsWriter {
    conn: Connection,
}

impl StatsWriter {
    /// Open (creating and migrating if needed) the database in `config`.
    pub fn open(config: &StatsConfig) -> Result<StatsWriter, StatsError> {
        Ok(StatsWriter {
            conn: schema::open_read_write(&config.path)?,
        })
    }

    /// Empty the copy of the settings on requests from before
    /// `older_than_ms`, a bounded number per call, and return how many.
    /// See [`crate::purge`].
    pub fn trim_contexts(&mut self, older_than_ms: i64) -> Result<u64, StatsError> {
        let tx = self.conn.transaction()?;
        let trimmed = crate::purge::trim_contexts(&tx, older_than_ms, CONTEXTS_PER_PASS)?;
        tx.commit()?;
        Ok(trimmed)
    }

    /// The underlying connection, for maintenance queries.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The underlying connection, mutably, for statements that need their own
    /// transaction (see [`StatsWriter::prune`]).
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Insert `events` in one transaction, returning how many rows landed.
    ///
    /// All or nothing: a batch that fails leaves the database exactly as it
    /// was, which is why a panicking or erroring writer can never produce a
    /// half-written request.
    pub fn write_batch(&mut self, events: &[Event]) -> Result<usize, StatsError> {
        if events.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        for event in events {
            match event {
                Event::Optimization(e) => {
                    insert_context(
                        &tx,
                        &e.tenant,
                        &e.request_id,
                        e.time_ms,
                        e.model.as_deref(),
                        Some(e.mode.as_str()),
                        e.context.as_ref(),
                    )?;
                    insert_optimization(&tx, e)?;
                }
                Event::Retrieval(e) => insert_retrieval(&tx, e)?,
                Event::ProtectedRefs(e) => insert_protected(&tx, e)?,
                Event::ProxyRequest(e) => {
                    insert_context(
                        &tx,
                        &e.tenant,
                        &e.request_id,
                        e.time_ms,
                        e.model.as_deref(),
                        None,
                        e.context.as_ref(),
                    )?;
                    insert_proxy(&tx, e)?;
                }
                Event::Discarded { tenant, request_id } => {
                    // The row keeps the engine's own numbers — that is what the
                    // engine did — and gains an outcome saying the result never
                    // left the machine. `v_daily_savings` and
                    // `v_request_economy` exclude it; `v_outcomes` counts it.
                    tx.prepare_cached(
                        "UPDATE optimization_events SET outcome = 'discarded'
                         WHERE tenant = ?1 AND request_id = ?2",
                    )?
                    .execute(params![tenant, request_id])?;
                }
                Event::Backup(e) => insert_backup(&tx, e)?,
                Event::Maintenance(e) => insert_maintenance(&tx, e)?,
                Event::StoreSample {
                    time_ms,
                    live_refs,
                    live_bytes,
                } => {
                    // the sweep runs on a timer, and two samples in the same
                    // millisecond are the same measurement; replacing is right
                    // and, more to the point, a primary-key conflict here would
                    // abort a whole batch of unrelated events
                    tx.prepare_cached(
                        "INSERT OR REPLACE INTO store_samples (time_ms, live_refs, live_bytes) VALUES (?1, ?2, ?3)",
                    )?
                    .execute(params![time_ms, live_refs, live_bytes])?;
                }
            }
        }
        tx.commit()?;
        Ok(events.len())
    }
}

fn insert_optimization(conn: &Connection, e: &OptimizationEvent) -> Result<(), StatsError> {
    let content_types = serde_json::to_string(&e.content_types)?;
    let protected = serde_json::to_string(&e.protected_by_reason)?;
    let skipped = serde_json::to_string(&e.skipped_by_reason)?;
    let skipped_tokens = serde_json::to_string(&e.skipped_tokens_by_reason)?;
    conn.prepare_cached(
        "INSERT INTO optimization_events (
            time_ms, tenant, request_id, surface, api_family, model, provider, mode,
            tokens_in, tokens_out, chars_in, chars_out,
            messages_total, messages_compressed, messages_skipped,
            refs_emitted, ref_bytes_emitted, content_types, protected_by_reason, engine_ms,
            outcome, skipped_by_reason, frozen_messages, skipped_tokens_by_reason)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
    )?
    .execute(params![
        e.time_ms,
        e.tenant,
        e.request_id,
        e.surface.as_str(),
        e.api_family,
        e.model,
        e.provider,
        e.mode.as_str(),
        i64::from(e.tokens_in),
        i64::from(e.tokens_out),
        // SQLite integers are signed 64-bit; a u64 character count that did not
        // fit would mean a single request carried eight exabytes of text
        e.chars_in as i64,
        e.chars_out as i64,
        i64::from(e.messages_total),
        i64::from(e.messages_compressed),
        i64::from(e.messages_skipped),
        i64::from(e.refs_emitted),
        e.ref_bytes_emitted as i64,
        content_types,
        protected,
        e.engine_ms,
        e.outcome.as_deref(),
        skipped,
        e.frozen_messages,
        skipped_tokens,
    ])?;
    Ok(())
}

fn insert_retrieval(conn: &Connection, e: &RetrievalEvent) -> Result<(), StatsError> {
    conn.prepare_cached(
        "INSERT INTO retrieval_events (
            time_ms, tenant, request_id, surface, \"ref\", found, expired, ranged,
            fragment_bytes, fragment_chars, fragment_tokens, returned_chars, returned_tokens,
            withheld, context_json, range_status)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
    )?
    .execute(params![
        e.time_ms,
        e.tenant,
        e.request_id,
        e.surface.as_str(),
        e.reference,
        i64::from(e.found),
        i64::from(e.expired),
        i64::from(e.ranged),
        e.fragment_bytes as i64,
        e.fragment_chars as i64,
        i64::from(e.fragment_tokens),
        e.returned_chars as i64,
        i64::from(e.returned_tokens),
        i64::from(e.withheld),
        e.context.as_ref().map(serde_json::to_string).transpose()?,
        e.range_status,
    ])?;
    Ok(())
}

fn insert_protected(conn: &Connection, e: &ProtectedRefEvent) -> Result<(), StatsError> {
    let refs = serde_json::to_string(&e.refs)?;
    let reasons = serde_json::to_string(&e.reasons)?;
    conn.prepare_cached(
        "INSERT INTO protected_ref_events (
            time_ms, tenant, request_id, refs, refs_total, reasons, messages, touched)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
    )?
    .execute(params![
        e.time_ms,
        e.tenant,
        e.request_id,
        refs,
        e.refs.len() as i64,
        reasons,
        i64::from(e.messages),
        i64::from(e.touched),
    ])?;
    Ok(())
}

fn insert_context(
    conn: &Connection,
    tenant: &str,
    request_id: &str,
    time: i64,
    model: Option<&str>,
    mode: Option<&str>,
    context: Option<&crate::context::RunContext>,
) -> Result<(), StatsError> {
    let json = context.map(serde_json::to_string).transpose()?;
    conn.execute("INSERT INTO request_context(tenant,request_id,time_ms,model,mode,context) VALUES (?1,?2,?3,?4,?5,?6)
    ON CONFLICT(tenant,request_id) DO UPDATE SET model=coalesce(excluded.model,request_context.model),mode=coalesce(excluded.mode,request_context.mode),context=coalesce(request_context.context,excluded.context)",params![tenant,request_id,time,model,mode,json])?;
    Ok(())
}

fn insert_proxy(conn: &Connection, e: &ProxyRequestEvent) -> Result<(), StatsError> {
    conn.prepare_cached(
        "INSERT INTO proxy_requests (
            time_ms, tenant, request_id, api_family, model, upstream, status, stream,
            prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens,
            shadow_prompt_tokens, retrieve_rounds, retrieve_tokens_injected,
            upstream_ms, total_ms, error, conversation, strip_usage, cache_write_1h_tokens,
            keep_warm, timing_json, prefix_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
    )?
    .execute(params![
        e.time_ms,
        e.tenant,
        e.request_id,
        e.api_family,
        e.model,
        e.upstream,
        e.status.map(i64::from),
        i64::from(e.stream),
        e.prompt_tokens.map(i64::from),
        e.completion_tokens.map(i64::from),
        e.cache_read_tokens.map(i64::from),
        e.cache_write_tokens.map(i64::from),
        e.shadow_prompt_tokens.map(i64::from),
        i64::from(e.retrieve_rounds),
        i64::from(e.retrieve_tokens_injected),
        e.upstream_ms,
        e.total_ms,
        e.error,
        e.conversation,
        i64::from(e.strip_usage),
        e.cache_write_1h_tokens.map(i64::from),
        i64::from(e.keep_warm),
        e.timing.as_ref().map(serde_json::to_string).transpose()?,
        e.prefix.as_ref().map(serde_json::to_string).transpose()?,
    ])?;
    // `compression_event_id` stays NULL. The rowid it would point at may not
    // exist yet — the proxy can finish a request before the compression event
    // it queued has been committed — and every query that needs the link uses
    // `request_id`, which both rows carry from the start.
    Ok(())
}

/// Ownership of the writer thread.
///
/// Dropping it shuts the thread down and waits for the final commit, so a
/// caller that forgets to call [`StatsHandle::shutdown`] still loses nothing.
pub struct StatsHandle {
    tx: Option<SyncSender<Msg>>,
    thread: Option<JoinHandle<()>>,
}

impl StatsHandle {
    /// Commit what is queued, stop the thread and wait for it.
    pub fn shutdown(mut self) -> Result<(), StatsError> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), StatsError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        if let Some(tx) = self.tx.take() {
            // a blocking send: shutdown is allowed to wait for a full queue,
            // and the alternative is discarding everything still in it
            let _ = tx.send(Msg::Shutdown);
        }
        thread.join().map_err(|_| StatsError::WriterPanicked)
    }
}

impl Drop for StatsHandle {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            tracing::warn!(error = %e, "stats writer did not shut down cleanly");
        }
    }
}

/// Start the writer thread and return a sink to feed it.
///
/// With `config.enabled` false this opens nothing and starts nothing: the sink
/// is [`null_sink`] and the handle is inert.
pub fn spawn(config: &StatsConfig) -> Result<(StatsSink, StatsHandle), StatsError> {
    if !config.enabled {
        return Ok((
            null_sink(),
            StatsHandle {
                tx: None,
                thread: None,
            },
        ));
    }

    // opened on the calling thread so a bad path or an unmigratable file is a
    // startup error the operator sees, not a thread that dies silently
    let writer = StatsWriter::open(config)?;

    let (tx, rx) = mpsc::sync_channel(config.channel_capacity);
    let flush_interval = config.flush_interval;
    let batch_max = config.batch_max.max(1);
    let retention = config.clone();
    let dropped = Arc::new(AtomicU64::new(0));
    let thread_dropped = Arc::clone(&dropped);
    let upkeep = Arc::new(Mutex::new(None));
    let thread_upkeep = Arc::clone(&upkeep);
    let thread = std::thread::Builder::new()
        .name("kompressor-stats".to_string())
        .spawn(move || {
            run(
                writer,
                rx,
                flush_interval,
                batch_max,
                retention,
                &thread_dropped,
                &thread_upkeep,
            )
        })
        .map_err(StatsError::Thread)?;

    let sink = StatsSink {
        context: None,
        inner: Some(Arc::new(SinkInner {
            tx: tx.clone(),
            dropped,
            upkeep,
        })),
    };
    Ok((
        sink,
        StatsHandle {
            tx: Some(tx),
            thread: Some(thread),
        },
    ))
}

/// How often retention runs. Hourly: the unit of work is a whole UTC day, so
/// anything faster re-asks a question whose answer cannot have changed.
const UPKEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Requests whose settings one hourly pass empties: a busy day's worth, so a
/// first pass over months of history catches up within a day of uptime
/// without one transaction holding the writer for long.
const CONTEXTS_PER_PASS: usize = 50_000;

/// The drain loop: commit on whichever comes first, the interval or the batch
/// cap.
fn run(
    mut writer: StatsWriter,
    rx: Receiver<Msg>,
    flush_interval: Duration,
    batch_max: usize,
    retention: StatsConfig,
    dropped: &AtomicU64,
    last_upkeep: &Mutex<Option<Upkeep>>,
) {
    let mut batch: Vec<Event> = Vec::with_capacity(batch_max);
    let mut deadline = Instant::now() + flush_interval;

    // Deliberately `now()`: a process that starts, prunes nothing and exits
    // would never retire anything, so the first pass runs before the first
    // event. Safe to do at startup only because `maintain` caps the days it
    // archives per pass — unbounded, this was a stall in the one place nothing
    // should stall, with a long backlog behind it.
    let mut upkeep = Instant::now();
    loop {
        if Instant::now() >= upkeep {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let cutoff = retention.retention_cutoff_ms(now_ms);
            let context_cutoff = retention.context_cutoff_ms(now_ms);
            if cutoff.is_some() || context_cutoff.is_some() {
                let started = Instant::now();
                let result = cutoff.map(|cutoff| writer.maintain(cutoff));
                if let Some(Err(error)) = &result {
                    tracing::warn!(%error, "statistics retention failed");
                }
                let trimmed = context_cutoff.map(|cutoff| writer.trim_contexts(cutoff));
                if let Some(Err(error)) = &trimmed {
                    tracing::warn!(%error, "trimming request settings failed");
                }
                let error = [
                    result.as_ref().and_then(|r| r.as_ref().err()),
                    trimmed.as_ref().and_then(|r| r.as_ref().err()),
                ]
                .into_iter()
                .flatten()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
                *last_upkeep
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Upkeep {
                    at_ms: now_ms,
                    took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    removed: result.and_then(Result::ok),
                    contexts_trimmed: trimmed.and_then(Result::ok),
                    error: (!error.is_empty()).then(|| error.join("; ")),
                });
            }
            upkeep = Instant::now() + UPKEEP_INTERVAL;
        }
        let timeout = deadline
            .min(upkeep)
            .saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(Msg::Event(event)) => {
                batch.push(*event);
                if batch.len() >= batch_max {
                    log_failure(commit(&mut writer, &mut batch, dropped));
                    deadline = Instant::now() + flush_interval;
                }
            }
            Ok(Msg::Flush(ack)) => {
                // the queue is FIFO, so everything sent before this message is
                // already in `batch`: acknowledging after the commit is an
                // honest "it is durable"
                let result = commit(&mut writer, &mut batch, dropped);
                let report = result.as_ref().map(|_| ()).map_err(StatsError::to_string);
                log_failure(result);
                let _ = ack.send(report);
                deadline = Instant::now() + flush_interval;
            }
            Ok(Msg::Exclusive(job)) => {
                log_failure(commit(&mut writer, &mut batch, dropped));
                job();
                deadline = Instant::now() + flush_interval;
            }
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                log_failure(commit(&mut writer, &mut batch, dropped));
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                log_failure(commit(&mut writer, &mut batch, dropped));
                deadline = Instant::now() + flush_interval;
            }
        }
    }
}

fn commit(
    writer: &mut StatsWriter,
    batch: &mut Vec<Event>,
    dropped: &AtomicU64,
) -> Result<(), StatsError> {
    if batch.is_empty() {
        return Ok(());
    }
    let result = writer.write_batch(batch).map(|_| ());
    if result.is_err() {
        // those events are gone for good: count them, so the drop metric covers
        // a failed commit as well as a full queue
        dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
    }
    // the batch is cleared either way: retrying a transaction SQLite rejected
    // would stall the queue behind a row that is never going to be accepted,
    // and blocking the queue is the one failure mode this crate must not have
    batch.clear();
    result
}

fn log_failure(result: Result<(), StatsError>) {
    if let Err(e) = result {
        tracing::warn!(error = %e, "dropping a stats batch that would not commit");
    }
}

/// File one backup. Shared by the writer and by [`record_backup`].
fn insert_backup(
    conn: &rusqlite::Connection,
    event: &crate::events::BackupEvent,
) -> Result<(), StatsError> {
    conn.prepare_cached(
        "INSERT INTO backups (time_ms, origin, destination, bytes, databases, ok, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        event.time_ms,
        event.origin,
        event.destination,
        event.bytes,
        event.databases,
        event.ok,
        event.error,
    ])?;
    Ok(())
}

/// Record a backup straight into the statistics database at `path`, for a
/// process that has no writer of its own: `kompressor backup`.
///
/// Returns whether it was recorded. A database that is not there is not
/// created for the sake of one row, and one that has not been migrated this
/// far is left alone — a backup must not be what upgrades the schema under a
/// running service. Either is `Ok(false)`: the backup itself succeeded, and
/// the caller says the record could not be kept.
pub fn record_backup(
    path: &std::path::Path,
    event: &crate::events::BackupEvent,
) -> Result<bool, StatsError> {
    if !path.is_file() {
        return Ok(false);
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'backups'",
        [],
        |row| row.get(0),
    )?;
    if has_table == 0 {
        return Ok(false);
    }
    insert_backup(&conn, event)?;
    Ok(true)
}

/// File one piece of maintenance work. Shared by the writer and by
/// [`record_maintenance`].
fn insert_maintenance(
    conn: &rusqlite::Connection,
    event: &crate::events::MaintenanceEvent,
) -> Result<(), StatsError> {
    conn.prepare_cached(
        "INSERT INTO maintenance_events (time_ms, action, target, origin, actor, took_ms, ok,
                                         bytes_before, bytes_after, detail, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        event.time_ms,
        event.action,
        event.target,
        event.origin,
        event.actor,
        event.took_ms,
        event.ok,
        event.bytes_before,
        event.bytes_after,
        event.detail,
        event.error,
    ])?;
    Ok(())
}

/// Record maintenance work straight into the statistics database at `path`,
/// for a process that has no writer of its own: `kompressor db …`.
///
/// Returns whether it was recorded, under the rule [`record_backup`]
/// follows: a database that is not there, or not yet migrated this far, is
/// left alone and the answer is `Ok(false)`.
pub fn record_maintenance(
    path: &std::path::Path,
    event: &crate::events::MaintenanceEvent,
) -> Result<bool, StatsError> {
    if !path.is_file() {
        return Ok(false);
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    if !crate::schema::has_table(&conn, "maintenance_events")? {
        return Ok(false);
    }
    insert_maintenance(&conn, event)?;
    Ok(true)
}
