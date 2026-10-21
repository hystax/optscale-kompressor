# Intercept listener: replacing the mitmproxy adapter

A sketch, not an implementation. It covers the module layout, the config
surface and the decisions worth settling before any code is written.

## What this replaces

[CLAUDE-DESKTOP.md](../CLAUDE-DESKTOP.md) currently needs a Python runtime,
mitmproxy, a `config.yaml`, an addon and a second service unit. All of it exists
to do four things:

1. accept `CONNECT host:443` from a client configured to use a proxy
2. terminate TLS for one allow-listed host, using a locally trusted CA
3. tunnel every other host untouched
4. hand the decrypted request to Kompressor on 8817

Only the first three are missing from this binary. The fourth is already here:
`compose(&core, selected, client)` in `kompressor-cli/src/serve.rs` returns an
`axum::Router`, and `axum::serve` only feeds it a listener. Anything that
yields a byte stream can serve the same router.

![The intercept accept path](../charts/intercept.drawio.png)

The chart is a `.drawio.png`: the picture and its source are the same file, so
opening it in drawio edits both. Re-export with `-e` or the source is lost.

## Module layout

A new module tree inside `kompressor-proxy`, beside the existing nineteen
files, rather than a new crate: it shares `ProxyConfig`, the router and the
upstream table, and a crate boundary between them would buy nothing.

```
crates/kompressor-proxy/src/intercept/
    mod.rs        listener, accept loop, the CONNECT state machine
    connect.rs    CONNECT request parsing and the 200 / 4xx replies
    tunnel.rs     blind relay for hosts that are not intercepted
    ca.rs         CA load-or-create, leaf minting, the in-memory cache
    allow.rs      host matching, and the refusal when nothing matches
```

Five files, none of them large. `mod.rs` is the only one the rest of the crate
sees.

### The accept path

```
TcpListener (proxy.intercept.listen)
  └─ read request line
       ├─ not CONNECT ......................... 405, close
       ├─ CONNECT, host not allow-listed ...... 200, tunnel.rs, copy_bidirectional
       └─ CONNECT, host allow-listed .......... 200, then:
            ca.rs mints a leaf for the SNI (cached)
            tokio_rustls::TlsAcceptor handshake
            hyper_util::server::conn::auto serves the SAME axum Router
            request arrives at the existing handler, routed by path as usual
```

The intercepted branch rewrites nothing. A request for
`api.anthropic.com/v1/messages` arrives at the router as `/v1/messages`, so the
mount that serves it is an ordinary `[[proxy.upstreams]]` entry with
`path_prefix = "/v1"` — the same one a wrapped CLI uses. The adapter's
rewrite to `/anthropic/v1/messages` exists only because mitmproxy had to name a
URL on another port; nothing needs that indirection once the router is in
process.

### Where it hooks in

`serve.rs` binds its surfaces from a `groups` map and spawns one
`axum::serve` per address. The intercept listener is a sibling task spawned
from the same `JoinSet`, sharing `shutdown_rx`, so `shutdown_grace` already
covers it. It is not a `selected` surface: it serves no routes of its own, it
only produces connections that the proxy surface's router then handles.

## Config surface

A new table, defaulting to off, following `RefsConfig` in shape: `#[serde(default,
deny_unknown_fields)]`, a hand-written `Default`, and the usual
`KOMPRESSOR__PROXY__INTERCEPT__*` overlay for free.

```toml
[proxy.intercept]
# Off unless a listener is named. No default port: a CONNECT proxy that
# appears because someone upgraded is not a pleasant surprise.
listen = "127.0.0.1:8840"

# Exact hosts to terminate TLS for. Everything else is tunnelled blind.
# No globs, no regex: see "Decisions" below.
intercept_hosts = ["api.anthropic.com"]

# Where the CA lives. Created on first start if absent, 0600 on the key.
ca_path = "/home/you/.local/share/kompressor/ca"

# What to advertise in ALPN. Must be measured against the client.
alpn = ["http/1.1"]

# Leaf certificates are minted per host and cached in memory. This bounds
# that cache; it is not a disk cache.
leaf_cache_entries = 64
```

```rust
pub struct InterceptConfig {
    pub listen: Option<SocketAddr>,
    pub intercept_hosts: Vec<String>,
    pub ca_path: PathBuf,
    pub alpn: Vec<String>,
    pub leaf_cache_entries: usize,
}
```

`listen: Option<SocketAddr>` rather than a bool plus an address: one field that
cannot contradict itself, and `None` is the default.

## Decisions worth settling first

**ALPN is the only real unknown.** The current `config.yaml` does not disable
mitmproxy's http2, so Desktop may well be negotiating h2 today. Advertising
only `http/1.1` and letting the client downgrade is the cheap path and is
usually accepted; `hyper_util`'s auto-builder can serve h2 if it is not. This
must be measured against Desktop before the shape is fixed, because it decides
whether `alpn` stays a config knob or becomes a constant.

**Exact hosts, not patterns.** mitmproxy's `allow_hosts` takes a regex, and
`'^api\.anthropic\.com:443$'` is doing load-bearing anchoring. A CONNECT proxy
that intercepts more than it meant to is a much larger security object than one
that tunnels everything else blindly, and a mis-anchored regex is how that
happens quietly. Exact hostname equality, lowercased, with the port implied by
CONNECT, has no such failure. If patterns are ever wanted, they can be added
with the strictness already visible.

**The CA is a trust anchor on the user's machine.** Create it 0600, print its
path and fingerprint on creation, and document removal in the same breath as
installation — a local CA that outlives the experiment that needed it is the
part of this feature most likely to be regretted. Minting leaves for allow-listed
hosts only means a stolen key still cannot be used against arbitrary hosts
through this proxy, though it can be used anywhere the CA is trusted; that is
worth saying plainly in the docs.

**Nothing changes about trust installation.** The app must still trust the CA
and must still be pointed at the proxy. Removing Python removes a runtime, a
pip install, a YAML file and a service unit from the install story — it does
not remove those two steps, and the documentation should not imply otherwise.

## What it costs

One new dependency, `rcgen`, for CA and leaf generation. `rustls` and
`tokio-rustls` are already in the tree, pulled in by `reqwest`; this would make
the server side of that a direct dependency.

## Testing without a browser

The accept path is testable offline, which is most of the value of keeping the
allow-list dumb:

- `connect.rs` — request-line parsing, including the malformed cases, as unit
  tests with no sockets
- `allow.rs` — matching, case folding, and that a near-miss hostname tunnels
  rather than intercepts
- `ca.rs` — create, reload, and that a second start does not mint a second CA
- end to end — a `reqwest` client configured with the proxy and the CA as a
  root, against a local upstream, asserting that an intercepted host reaches
  the router and a non-intercepted one does not
