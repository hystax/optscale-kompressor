//! What the running process knows about itself and holds nowhere else: the
//! addresses it is bound to and the requests it is in the middle of.
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

/// The process's own record of itself. Empty until someone fills it in: a
/// core built for a one-off command has no listeners and carries no requests.
#[derive(Debug, Default)]
pub struct Runtime {
    listeners: Mutex<Vec<Listener>>,
    requests: AtomicUsize,
    streams: AtomicUsize,
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
}
