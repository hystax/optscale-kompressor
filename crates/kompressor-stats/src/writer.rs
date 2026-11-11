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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What travels down the queue. Control messages share it with events so a
/// flush cannot overtake the events it is meant to flush.
pub(crate) enum Msg {
    /// Append this.
    Event(Box<Event>),
    /// Commit everything queued before this, then acknowledge.
    Flush(mpsc::Sender<Result<(), String>>),
    /// Commit and stop. Anything queued behind this is discarded, so a caller
    /// with events it cares about flushes before shutting down.
    Shutdown,
}

struct SinkInner {
    tx: SyncSender<Msg>,
    dropped: AtomicU64,
}

/// A cloneable handle for recording events. Cheap to clone, safe to share.
#[derive(Clone)]
pub struct StatsSink {
    // `None` is the null sink: no thread, no file, no counter worth reading.
    inner: Option<Arc<SinkInner>>,
}

impl StatsSink {
    /// Record `event`, or drop it if the queue is full.
    ///
    /// Never blocks and never returns an error, by design; see the module doc.
    /// Use [`StatsSink::dropped`] to find out whether anything was lost.
    pub fn send(&self, event: Event) {
        let Some(inner) = &self.inner else {
            return;
        };
        match inner.tx.try_send(Msg::Event(Box::new(event))) {
            Ok(()) => {}
            // Full: the writer is behind. Disconnected: the writer is gone.
            // Neither is a reason to make the caller wait or care.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Events lost to a full or dead queue since start: `stats_dropped_total`.
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
}

/// A sink that discards everything, for `stats.enabled = false`.
///
/// Deliberately the same type rather than a trait object: disabling stats
/// should cost a branch on an `Option`, not a virtual call at every call site,
/// and callers should not need a feature flag or a generic parameter to say
/// "no analytics".
pub fn null_sink() -> StatsSink {
    StatsSink { inner: None }
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
            inner: Some(Arc::new(SinkInner {
                tx,
                dropped: AtomicU64::new(0),
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
            inner: Some(Arc::new(SinkInner {
                tx,
                dropped: AtomicU64::new(0),
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
                Event::Optimization(e) => insert_optimization(&tx, e)?,
                Event::Retrieval(e) => insert_retrieval(&tx, e)?,
                Event::ProtectedRefs(e) => insert_protected(&tx, e)?,
                Event::ProxyRequest(e) => insert_proxy(&tx, e)?,
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
            withheld)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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

fn insert_proxy(conn: &Connection, e: &ProxyRequestEvent) -> Result<(), StatsError> {
    conn.prepare_cached(
        "INSERT INTO proxy_requests (
            time_ms, tenant, request_id, api_family, model, upstream, status, stream,
            prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens,
            shadow_prompt_tokens, retrieve_rounds, retrieve_tokens_injected,
            upstream_ms, total_ms, error, conversation, strip_usage, cache_write_1h_tokens,
            keep_warm)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
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
    let thread = std::thread::Builder::new()
        .name("kompressor-stats".to_string())
        .spawn(move || run(writer, rx, flush_interval, batch_max, retention))
        .map_err(StatsError::Thread)?;

    let sink = StatsSink {
        inner: Some(Arc::new(SinkInner {
            tx: tx.clone(),
            dropped: AtomicU64::new(0),
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

/// The drain loop: commit on whichever comes first, the interval or the batch
/// cap.
fn run(
    mut writer: StatsWriter,
    rx: Receiver<Msg>,
    flush_interval: Duration,
    batch_max: usize,
    retention: StatsConfig,
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
            if let Some(cutoff) = retention.retention_cutoff_ms(now_ms)
                && let Err(error) = writer.maintain(cutoff)
            {
                tracing::warn!(%error, "statistics retention failed");
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
                    log_failure(commit(&mut writer, &mut batch));
                    deadline = Instant::now() + flush_interval;
                }
            }
            Ok(Msg::Flush(ack)) => {
                // the queue is FIFO, so everything sent before this message is
                // already in `batch`: acknowledging after the commit is an
                // honest "it is durable"
                let result = commit(&mut writer, &mut batch);
                let report = result.as_ref().map(|_| ()).map_err(StatsError::to_string);
                log_failure(result);
                let _ = ack.send(report);
                deadline = Instant::now() + flush_interval;
            }
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                log_failure(commit(&mut writer, &mut batch));
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                log_failure(commit(&mut writer, &mut batch));
                deadline = Instant::now() + flush_interval;
            }
        }
    }
}

fn commit(writer: &mut StatsWriter, batch: &mut Vec<Event>) -> Result<(), StatsError> {
    if batch.is_empty() {
        return Ok(());
    }
    let result = writer.write_batch(batch).map(|_| ());
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
