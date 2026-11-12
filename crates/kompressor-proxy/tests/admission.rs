//! What bounds the proxy's engine work, and what a slow fragment store can and
//! cannot do to the requests around it.
//!
//! Both halves come from the 2026-11-11 review. F1: the compression deadline
//! covered the computation only, and the SQLite write that retains fragments
//! ran afterwards on the async worker — a store behind its busy timeout held
//! that worker, every unrelated stream on it, and the timers meant to bound
//! it. F2: every request spawned its own blocking task with no admission at
//! all, and a timed-out task is not cancelled, so a burst queued and ran
//! without limit after every caller had already fallen open.
//!
//! The store here is the instrument: a [`MemoryRefStore`] whose writes can be
//! made to stall, and which counts how many writes are in flight at once.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kompressor_engine::refs::RefId;
use kompressor_proxy::failopen::Stage;
use kompressor_stats::events::Event;
use kompressor_store::memory::MemoryRefStore;
use kompressor_store::types::{Lookup, PutMeta, RefStore, StoreError, StoreStats, Tenant};
use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use support::{Harness, chat_body, log_payload};

/// A store whose writes take `delay` while `slow` is set.
#[derive(Default)]
struct StallingStore {
    inner: MemoryRefStore,
    slow: AtomicBool,
    delay_ms: AtomicUsize,
    writes: AtomicUsize,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
}

impl StallingStore {
    fn stalling(delay: Duration) -> Arc<StallingStore> {
        let store = StallingStore::default();
        store.slow.store(true, Ordering::SeqCst);
        store
            .delay_ms
            .store(delay.as_millis() as usize, Ordering::SeqCst);
        Arc::new(store)
    }
}

impl RefStore for StallingStore {
    fn put_many(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<(), StoreError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_in_flight.fetch_max(now, Ordering::SeqCst);
        if self.slow.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(
                self.delay_ms.load(Ordering::SeqCst) as u64
            ));
        }
        let result = self.inner.put_many(tenant, refs, meta);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.writes.fetch_add(1, Ordering::SeqCst);
        result
    }

    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError> {
        self.inner.get(tenant, id, touch)
    }

    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        self.inner.touch_many(tenant, ids)
    }

    fn purge_expired(&self, now: u64) -> Result<u64, StoreError> {
        self.inner.purge_expired(now)
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        self.inner.stats()
    }

    fn stats_for(&self, tenant: &Tenant) -> Result<StoreStats, StoreError> {
        self.inner.stats_for(tenant)
    }
}

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "gpt-5",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11}
        })))
        .mount(&harness.server)
        .await;
}

/// Whether the tool result the provider received is the one the client sent.
fn forwarded_whole(upstream: &str, payload: &str) -> bool {
    let value: serde_json::Value = serde_json::from_str(upstream).expect("JSON upstream");
    value["messages"][3]["content"] == payload
}

fn optimizations(harness: &Harness) -> usize {
    harness
        .events
        .drain()
        .iter()
        .filter(|event| matches!(event, Event::Optimization(_)))
        .count()
}

/// A store that stalls costs the request its saving, never its latency, and
/// never anyone else's.
///
/// Single-threaded runtime on purpose: it is the deployment where a blocking
/// call on the async side stops *everything*. Before the fix the 1.5 s store
/// write ran on this one thread after the computation finished in time, so the
/// compressed request took 1.5 s and the unrelated small request queued behind
/// it — the timer it sleeps on could not even fire.
#[tokio::test(flavor = "current_thread")]
async fn a_stalled_store_falls_the_request_open_instead_of_stalling_the_runtime() {
    let store = StallingStore::stalling(Duration::from_millis(1_500));
    let harness = Harness::with_store(
        |config| config.proxy.compress_timeout = Duration::from_millis(300),
        store.clone(),
    )
    .await;
    answering(&harness).await;

    let payload = log_payload(400);
    let big = chat_body(&payload);
    let small = chat_body("a short tool result, well under the size gate");

    let started = Instant::now();
    let compressed = async {
        let reply = harness.post("/v1/chat/completions", &big).await;
        (reply.status, started.elapsed())
    };
    let unrelated = async {
        // Starts once the compression is under way, so it overlaps the write.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let began = Instant::now();
        let reply = harness.post("/v1/chat/completions", &small).await;
        (reply.status, began.elapsed())
    };
    let ((status, compressed_took), (unrelated_status, unrelated_took)) =
        tokio::join!(compressed, unrelated);

    assert_eq!((status.as_u16(), unrelated_status.as_u16()), (200, 200));
    assert!(
        compressed_took < Duration::from_millis(1_000),
        "the deadline bounds the store write too: {compressed_took:?}"
    );
    assert!(
        unrelated_took < Duration::from_millis(500),
        "an unrelated request waited on someone else's store write: {unrelated_took:?}"
    );
    assert_eq!(harness.failopen.count(Stage::Compress), 1);
    let bodies = harness.upstream_bodies().await;
    assert!(
        bodies.iter().any(|body| forwarded_whole(body, &payload)),
        "the timed-out request went upstream as it arrived"
    );

    // The abandoned task still finishes its write; what it must not do is
    // file a saving for an output that was never forwarded.
    tokio::time::sleep(Duration::from_millis(1_700)).await;
    assert!(
        store.writes.load(Ordering::SeqCst) >= 1,
        "the store was reached"
    );
    assert_eq!(
        optimizations(&harness),
        0,
        "an abandoned result recorded a compression"
    );
}

/// At the permit limit, a request is forwarded uncompressed rather than
/// queued behind the engine, and says why on its own fail-open stage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saturated_engine_forwards_the_original_instead_of_queueing() {
    let store = StallingStore::stalling(Duration::from_millis(1_000));
    let harness = Harness::with_store(
        |config| {
            config.server.max_inflight_compressions = 1;
            config.proxy.compress_timeout = Duration::from_millis(300);
        },
        store.clone(),
    )
    .await;
    answering(&harness).await;

    let first = log_payload(400);
    let second = log_payload(401);
    let (first_body, second_body) = (chat_body(&first), chat_body(&second));
    let started = Instant::now();
    let (a, b) = tokio::join!(harness.post("/v1/chat/completions", &first_body), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        harness.post("/v1/chat/completions", &second_body).await
    });
    let took = started.elapsed();

    assert_eq!((a.status.as_u16(), b.status.as_u16()), (200, 200));
    assert!(took < Duration::from_millis(900), "nobody queued: {took:?}");
    // The first holds the only permit in its stalled write and times out; the
    // second never gets the permit at all.
    assert_eq!(harness.failopen.count(Stage::Compress), 1);
    assert_eq!(harness.failopen.count(Stage::Admission), 1);
    let bodies = harness.upstream_bodies().await;
    assert!(bodies.iter().any(|body| forwarded_whole(body, &first)));
    assert!(bodies.iter().any(|body| forwarded_whole(body, &second)));

    // The permit comes back when the abandoned work ends — not before, and
    // not never. Once the store is fast again the next request compresses.
    store.slow.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let third = log_payload(402);
    let reply = harness
        .post("/v1/chat/completions", &chat_body(&third))
        .await;
    assert_eq!(reply.status, 200);
    let bodies = harness.upstream_bodies().await;
    assert!(
        !forwarded_whole(bodies.last().expect("a third request"), &third),
        "the engine is usable again once the abandoned work has finished"
    );
    assert_eq!(harness.failopen.count(Stage::Admission), 1);
}

/// However many requests arrive at once, no more than the permit count run
/// the engine together — the store sees at most that many writes in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_never_run_more_engine_work_than_there_are_permits() {
    let store = StallingStore::stalling(Duration::from_millis(150));
    let harness = Harness::with_store(
        |config| {
            config.server.max_inflight_compressions = 2;
            config.proxy.compress_timeout = Duration::from_secs(5);
        },
        store.clone(),
    )
    .await;
    answering(&harness).await;

    // Separate tasks, as separate connections are: polled together on one
    // task, the requests would serialise themselves and prove nothing.
    let tasks: Vec<_> = (0..6)
        .map(|i| {
            let router = harness.router();
            let body = chat_body(&log_payload(400 + i)).to_string();
            tokio::spawn(async move {
                use tower::ServiceExt;
                let request = http::Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .expect("a request");
                let response = router.oneshot(request).await.expect("the router answers");
                let status = response.status();
                // drain it, as a client would
                http_body_util::BodyExt::collect(response.into_body())
                    .await
                    .expect("a body");
                status
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.expect("the request task").as_u16(), 200);
    }
    assert_eq!(
        store.writes.load(Ordering::SeqCst),
        6,
        "every request compressed"
    );
    assert!(
        store.most_in_flight.load(Ordering::SeqCst) <= 2,
        "{} writes ran at once against 2 permits",
        store.most_in_flight.load(Ordering::SeqCst)
    );
    assert_eq!(harness.failopen.count(Stage::Admission), 0);
    assert_eq!(harness.failopen.count(Stage::Compress), 0);
}
