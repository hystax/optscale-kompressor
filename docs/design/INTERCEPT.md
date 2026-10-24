# Intercept listener: how it is built

The module behind `[proxy.intercept]`, as built. [CLAUDE-DESKTOP.md](../CLAUDE-DESKTOP.md)
is how to use it; this is the layout, the decisions, and what they cost.

## What it does

A client that reads `ANTHROPIC_BASE_URL` needs none of this. Claude Desktop
sets that variable itself when it spawns a Code-tab session, so there is no
value to set — but it honours a proxy setting and a certificate authority. This
listener is those four things:

1. accept `CONNECT host:443` from a client configured to use a proxy
2. terminate TLS for one allow-listed host, using a locally trusted CA
3. tunnel every other host untouched
4. hand the decrypted request to Kompressor on 8817

The fourth needed nothing new: `compose(&core, selected, client)` in
`kompressor-cli/src/serve.rs` returns an `axum::Router`, and `axum::serve` only
feeds it a listener. Anything that yields a byte stream can serve the same
router, which is why no mount, route or handler changed to support this.

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
`path_prefix = "/v1"` — the same one a wrapped CLI uses. A path-rewriting
mount such as `/anthropic` is never matched on this route and should not be
configured for it.

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

# What to advertise in ALPN. Desktop offers only h2; see "Decisions".
alpn = ["h2", "http/1.1"]

# Leaf certificates are minted per host and cached in memory. This bounds
# that cache; it is not a disk cache.
leaf_cache_entries = 64
```

```rust
pub struct InterceptConfig {
    pub listen: Option<String>,
    pub intercept_hosts: Vec<String>,
    pub ca_path: PathBuf,
    pub alpn: Vec<String>,
    pub leaf_cache_entries: usize,
}
```

`listen: Option<String>` rather than a bool plus an address: one field that
cannot contradict itself, and `None` is the default. It is a `String` because
the rest of the configuration spells addresses that way, and it is parsed —
with the error naming the key — when the listener binds.

## Decisions worth settling first

**ALPN was the one real unknown, and it bit.** Advertising only `http/1.1`
looked like the conservative choice — a client preferring h2 would downgrade.
Desktop's client does not: it offers **only** `h2` for `api.anthropic.com`, so
the listener answered `no_application_protocol` and the client hung up
mid-handshake. The log showed a handshake EOF, which is indistinguishable from
an untrusted certificate and sent the first person to hit it looking at trust
stores. `openssl s_client -alpn h2` settles it in one command: alert 120 with
`Verify return code: 0 (ok)` means trust is fine and the protocol was refused.
The default is now `["h2", "http/1.1"]`, and `hyper_util`'s auto-builder serves
either.

**Exact hosts, not patterns.** A CONNECT proxy that intercepts more than it
meant to is a much larger security object than one that tunnels everything else
blindly, and a pattern whose anchoring slips is how that happens quietly.
Exact hostname equality, lowercased, with the port implied by CONNECT, has no
such failure — `api.anthropic.com.evil.test` shares a prefix with an allowed
host and is not one, which a `starts_with` would get wrong in the dangerous
direction. There is a test for exactly that.

**The CA is a trust anchor on the user's machine.** Create it 0600, print its
path and fingerprint on creation, and document removal in the same breath as
installation — a local CA that outlives the experiment that needed it is the
part of this feature most likely to be regretted. Minting leaves for allow-listed
hosts only means a stolen key still cannot be used against arbitrary hosts
through this proxy, though it can be used anywhere the CA is trusted; that is
worth saying plainly in the docs.

**Trust installation is irreducible.** The app must trust the CA and must be
pointed at the proxy. Those two steps are inherent to intercepting a client
that pins its endpoint, and no amount of moving code around removes them.

## What it costs

`rcgen` for CA and leaf generation, `ring` for the fingerprint digest, `pem` for
one parse and `time` for the validity window. `rustls`, `tokio-rustls` and
`hyper-util` were already in the tree through `reqwest`; this makes the server
side of them direct dependencies.

Two defaults are worth knowing. Certificates are ECDSA P-256, not RSA. And the
validity window is set explicitly — three years for the CA, ninety days for a
leaf — because rcgen's default is 1975 to 4096, a 2121-year certificate that no
real authority issues and no trust store should be asked to accept.

## Testing without a browser

The accept path is testable offline, which is most of the value of keeping the
allow-list dumb. All of these exist:

- `connect.rs` — request-line parsing, including the malformed cases, as unit
  tests with no sockets
- `allow.rs` — matching, case folding, and that a near-miss hostname tunnels
  rather than intercepts
- `ca.rs` — create, reload, and that a second start does not mint a second CA
- end to end — a `reqwest` client configured with the proxy and the CA as a
  root, against a local upstream, asserting that an intercepted host reaches
  the router and a non-intercepted one does not
