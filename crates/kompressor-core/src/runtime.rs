//! What the running process knows about itself and holds nowhere else: the
//! addresses it is bound to, the requests it is in the middle of, what it
//! found in each database as it opened it, and what the proxy remembers only
//! in memory.
//!
//! It lives on [`crate::AppCore`] because the pieces that know these things
//! and the one that reports them are different crates: `serve` binds the
//! listeners, the proxy carries the requests, and the Maintenance page shows
//! both.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// One address the process is bound to and what it answers there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Listener {
    pub address: String,
    /// `api`, `mcp`, `proxy`, `statistics`, `interception`.
    pub serves: Vec<String>,
}

/// Proxied requests under way right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InFlight {
    /// Requests the proxy has taken and not yet finished answering.
    pub requests: usize,
    /// Those of them whose answer is a stream.
    pub streams: usize,
}

/// One database as this process found and left it when it opened it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Opened {
    /// `stats.db` or `refs.db`.
    pub database: String,
    /// The schema the file held before it was opened; `None` when there was
    /// no file, or none this build could read a version from.
    pub from: Option<u32>,
    /// The schema it holds now.
    pub to: u32,
    /// How long opening took, any migration included.
    pub took_ms: u64,
}

/// What the proxy holds in memory and a restart loses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Held {
    /// Conversations whose cache state is remembered.
    pub conversations: usize,
    /// Those of them with a frozen prefix: a point before which nothing is
    /// rewritten.
    pub frozen: usize,
    /// Those of them moved to one-hour cache entries by `cache_ttl = adaptive`.
    pub one_hour: usize,
    /// Conversations a keep-warm timer is running for.
    pub keep_warm: usize,
    /// Conversations holding a record of what a lapsed cache let be rewritten.
    pub recompacted: usize,
    /// Whether a lapse may still be predicted, or three wrong predictions in
    /// an hour have ended that until restart.
    pub predicting: bool,
    /// Predictions the provider's answer contradicted, since start.
    pub wrong_predictions: u64,
    /// Requests a stability guard (`proxy.guard_*`) tripped on, since start:
    /// the rewrite introduced a fault the provider refuses.
    pub guard_trips: u64,
    /// Of those, the ones under `enforce` that sent the client's original.
    pub guard_enforced: u64,
    /// Requests that went out uncompressed after an earlier turn of their
    /// conversation went out compressed, since start.
    pub failopen_after_compressed: u64,
}

/// Whoever holds the state [`Held`] counts, asked each time it is wanted.
struct Probe(Box<dyn Fn() -> Held + Send + Sync>);

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Probe")
    }
}

/// The process's own record of itself. Empty until someone fills it in: a
/// core built for a one-off command has no listeners and carries no requests.
#[derive(Debug, Default)]
pub struct Runtime {
    listeners: Mutex<Vec<Listener>>,
    requests: AtomicUsize,
    streams: AtomicUsize,
    opened: Mutex<Vec<Opened>>,
    held: Mutex<Option<Probe>>,
}

impl Runtime {
    /// Put a bound address on record. Whoever binds it calls this.
    pub fn listening(&self, address: impl Into<String>, serves: &[&str]) {
        let listener = Listener {
            address: address.into(),
            serves: serves.iter().map(|what| (*what).to_string()).collect(),
        };
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.push(listener);
        }
    }

    /// Every address on record, in the order they were bound.
    pub fn listeners(&self) -> Vec<Listener> {
        self.listeners
            .lock()
            .map(|listeners| listeners.clone())
            .unwrap_or_default()
    }

    /// Put a database's opening on record. Whoever opens it calls this.
    pub fn opened(&self, opened: Opened) {
        if let Ok(mut all) = self.opened.lock() {
            all.push(opened);
        }
    }

    /// Every database opening on record.
    pub fn openings(&self) -> Vec<Opened> {
        self.opened
            .lock()
            .map(|all| all.clone())
            .unwrap_or_default()
    }

    /// Say how to count what is held in memory. The proxy does, as it is
    /// built; the last to say so is the one asked.
    pub fn holding(&self, count: impl Fn() -> Held + Send + Sync + 'static) {
        if let Ok(mut held) = self.held.lock() {
            *held = Some(Probe(Box::new(count)));
        }
    }

    /// What is held in memory as this is called; `None` when nothing in this
    /// process keeps any, which is a process without the proxy.
    pub fn held(&self) -> Option<Held> {
        let held = self.held.lock().ok()?;
        held.as_ref().map(|probe| (probe.0)())
    }

    /// Count a proxied request in until the returned guard is dropped.
    pub fn enter(self: &Arc<Self>, stream: bool) -> Entered {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if stream {
            self.streams.fetch_add(1, Ordering::Relaxed);
        }
        Entered {
            runtime: self.clone(),
            stream,
        }
    }

    /// The proxied requests under way as this is called.
    pub fn in_flight(&self) -> InFlight {
        InFlight {
            requests: self.requests.load(Ordering::Relaxed),
            streams: self.streams.load(Ordering::Relaxed),
        }
    }
}

/// A request counted in by [`Runtime::enter`], counted out when dropped —
/// however the request ends, a client that hangs up included.
#[derive(Debug)]
pub struct Entered {
    runtime: Arc<Runtime>,
    stream: bool,
}

impl Drop for Entered {
    fn drop(&mut self) {
        self.runtime.requests.fetch_sub(1, Ordering::Relaxed);
        if self.stream {
            self.runtime.streams.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_counted_in_and_out_with_their_streams() {
        let runtime = Arc::new(Runtime::default());
        assert_eq!(runtime.in_flight(), InFlight::default());
        let plain = runtime.enter(false);
        let stream = runtime.enter(true);
        assert_eq!(
            runtime.in_flight(),
            InFlight {
                requests: 2,
                streams: 1
            }
        );
        drop(stream);
        assert_eq!(
            runtime.in_flight(),
            InFlight {
                requests: 1,
                streams: 0
            }
        );
        drop(plain);
        assert_eq!(runtime.in_flight(), InFlight::default());
    }

    #[test]
    fn listeners_are_kept_in_the_order_they_were_bound() {
        let runtime = Runtime::default();
        assert!(runtime.listeners().is_empty());
        runtime.listening("127.0.0.1:8817", &["api", "proxy"]);
        runtime.listening("127.0.0.1:8818", &["statistics"]);
        let seen = runtime.listeners();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].serves, ["api", "proxy"]);
        assert_eq!(seen[1].address, "127.0.0.1:8818");
    }

    #[test]
    fn openings_and_what_is_held_are_whatever_was_last_put_on_record() {
        let runtime = Runtime::default();
        assert_eq!((runtime.openings(), runtime.held()), (vec![], None));
        let opened = Opened {
            database: "stats.db".into(),
            from: Some(16),
            to: 17,
            took_ms: 12,
        };
        runtime.opened(opened.clone());
        assert_eq!(runtime.openings(), [opened]);
        let live = Arc::new(AtomicUsize::new(3));
        let counted = live.clone();
        runtime.holding(move || Held {
            conversations: counted.load(Ordering::Relaxed),
            predicting: true,
            ..Held::default()
        });
        assert_eq!(runtime.held().map(|held| held.conversations), Some(3));
        live.store(5, Ordering::Relaxed);
        assert_eq!(runtime.held().map(|held| held.conversations), Some(5));
    }
}
