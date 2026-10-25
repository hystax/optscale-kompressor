# Claude Desktop integration

Route the unmodified Claude Desktop **Code tab** through Kompressor using its
built-in intercept listener. Desktop retains its normal subscription login and
OAuth credentials; no separate API key is needed. Normal plan limits and any
enabled extra usage still apply. This guide covers Linux with systemd user
services.

Validated 2026-10-21 with Desktop's bundled Claude Code **2.1.295**.

```text
Claude Desktop Code → intercept listener 127.0.0.1:8840  (CONNECT + TLS)
                    → the proxy's own router, in the same process
                    → https://api.anthropic.com
```

One process. The listener terminates TLS for `api.anthropic.com` with a
certificate it signs locally, and tunnels every other host untouched; the
decrypted request is handled by the router Kompressor already serves on 8817.

**`HTTPS_PROXY` names the intercept port, not the proxy surface.** 8817 speaks
HTTP to clients that were pointed at it; 8840 is the CONNECT proxy a client
uses when it cannot be.

## Why Desktop needs intercepting

The CLI can use `kompressor wrap claude` or `ANTHROPIC_BASE_URL`. Signed-in
Desktop overrides that base URL, including values supplied through settings.
Desktop's third-party inference configuration instead uses separate gateway
credentials. Intercepting preserves the normal subscription authentication
path while allowing Kompressor to inspect Messages requests.

References:

- [Desktop-controlled environment variables](https://code.claude.com/docs/en/desktop#environment-variables-desktop-sets-itself)
- [Desktop gateway configuration](https://code.claude.com/docs/en/llm-gateway-connect#desktop-app)
- [Proxy and custom CA configuration](https://code.claude.com/docs/en/network-config)

## 1. Configure and start Kompressor

Use the following settings in `~/.config/kompressor/config.toml`. Merge them
into an existing configuration rather than duplicating TOML tables or replacing
other upstreams. Replace `/home/YOUR_USER` with your absolute home path; do not
assume TOML expands `~` or `$HOME`.

```toml
[server]
listen = "127.0.0.1:8817"

[engine]
mode = "recoverable"
max_ref_bytes = 1048576

[proxy]
frozen_prefix = "first_seen"

[refs]
backend = "sqlite"
path = "/home/YOUR_USER/.local/share/kompressor/refs.db"

[stats]
enabled = true
path = "/home/YOUR_USER/.local/share/kompressor/stats.db"

[proxy.intercept]
listen = "127.0.0.1:8840"
intercept_hosts = ["api.anthropic.com"]
ca_path = "/home/YOUR_USER/.local/share/kompressor/ca"
alpn = ["h2", "http/1.1"]
leaf_cache_entries = 64

[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/v1/messages"
base_url = "https://api.anthropic.com"
api_family = "anthropic"
forward_auth = true

[[proxy.upstreams]]
name = "anthropic-api"
path_prefix = "/api"
base_url = "https://api.anthropic.com"
api_family = "passthrough"
forward_auth = true
```

`[proxy.intercept]` is the listener that does this. It is off unless `listen`
names an address, so an upgrade never starts one by surprise.

The mounts are `/v1/messages` and `/api`, not `/anthropic`. An intercepted
request keeps the path the client sent, and the listener serves it from the same
process, so there is no rewriting and no second port. `/api` is required as well:
a Code session probes `/api/hello` and signs in through `/api/oauth/*`, and
without a mount for them it takes a 404 from the proxy mid-session.

`alpn` must include `h2`. Desktop's client offers **only** `h2` for
`api.anthropic.com`; a listener advertising just `http/1.1` answers
`no_application_protocol`, the client hangs up mid-handshake, and the log shows
a handshake EOF that looks exactly like an untrusted certificate. It is not
one — `openssl s_client -alpn h2` against such a listener reports alert 120
with `Verify return code: 0 (ok)`.

### Sharing the proxy with Codex

`/v1/messages` rather than `/v1`, because Codex wants `/v1` and cannot be told
otherwise: `kompressor wrap codex` writes `openai_base_url = ".../v1"` with no
key to override it. Upstream matching is longest-prefix, so the two coexist —
`/v1/messages` wins for Claude, `/v1/responses` falls through to OpenAI.

Do **not** reach for the obvious inverse and mount OpenAI on `/v1/responses`.
`rewrite_prefix` replaces the matched mount prefix, so that mount leaves an
empty tail and a subscription request arrives at `chatgpt.com` as
`/backend-api/codex/` — a 404 that reads like a broken login rather than a
routing mistake. Measured, twice, before the cause was obvious.

Put this mount **after** any `[proxy.upstreams.subscription]` block. TOML
attaches that table to the most recent `[[proxy.upstreams]]`, so a mount
inserted above one adopts it and forwards subscription traffic to the wrong
provider.

> **Since 1.1.0 the packaged default already carries all of this** — the mounts,
> `frozen_prefix`, `websocket_relay` and `[proxy.intercept]` itself. On a clean
> install there is nothing to add here: the listener comes up on 8840 and the
> certificate authority is generated on first start. Steps 2 and 3 below are
> still yours to do, because trusting that CA and pointing Desktop at the proxy
> are decisions a package cannot make for you.
>
> An upgrade never rewrites an existing config, so a machine that already had
> one keeps it and needs the edits above. dpkg leaves the new default at
> `/etc/kompressor/config.toml.dpkg-dist` to merge from.

`first_seen` opts into compressing history beyond the initially frozen prefix;
monitor cache usage as well as compression. Keep prompt caching enabled.

```bash
mkdir -p ~/.local/share/kompressor
kompressor serve --proxy --api --config ~/.config/kompressor/config.toml
```

Leave this running and use another terminal for the remaining steps. If an
existing Kompressor service already listens on 8817, use that instance instead
of launching a second one.

## 2. Trust the certificate authority

Kompressor creates its own CA the first time the intercept listener starts, in
the `ca_path` directory, and reuses it on every later start. Nothing has to be
generated by hand.

```bash
ls -l ~/.local/share/kompressor/ca/
```

```text
-rw-------  ca.key    the private key; never distribute it
-rw-rw-r--  ca.pem    the certificate to trust
```

The startup log names the file and its fingerprint, so the authority can be
recognised later in a trust store — and the right one removed when the
integration is retired:

```bash
journalctl --user -u kompressor -n 20 --no-pager | grep -i 'certificate authority'
```

The CA is valid for three years and the per-host certificates it signs for
ninety days. Those are minted on demand at handshake time, kept in memory only,
and never written to disk.


## 3. Configure Claude

Back up `~/.claude/settings.json`, then merge these entries into its `env`
object, preserving other settings. Replace `/home/YOUR_USER` with your absolute
home path; JSON does not expand shell variables.

```json
{
  "env": {
    "HTTPS_PROXY": "http://127.0.0.1:8840",
    "NODE_EXTRA_CA_CERTS": "/home/YOUR_USER/.local/share/kompressor/ca/ca.pem",
    "CLAUDE_CODE_TETHER_LIVE": "0"
  }
}
```

Remove any previous `ANTHROPIC_BASE_URL` override from these Desktop settings.
`NODE_EXTRA_CA_CERTS` is required in this setup because Claude must trust the
`HTTPS_PROXY` is what makes this work at all. Desktop overwrites
`ANTHROPIC_BASE_URL` when it spawns a session, so there is no base URL to set —
but it leaves the proxy and CA variables alone, and honours them.

`NODE_EXTRA_CA_CERTS` makes the client trust the locally signed certificate. It can be omitted only when that CA is
already trusted through a certificate store the client uses. Do not disable TLS
certificate verification.

**Fully quit and reopen Claude Desktop, then start a new local Code session.**

### Why disable Message Threads?

Bundled Claude Code 2.1.295 enables `message-threads-2026-08-12`: it sends a
`thread` object and only new messages while Anthropic holds the prior history.
The proxy cannot compress old tool results that never arrive again. Occasional
full-history requests still compress, making a routing-only test misleading.

`CLAUDE_CODE_TETHER_LIVE=0` disables Message Threads while retaining prompt
caching. This is an internal, version-specific switch, not a documented stable
interface. Revalidate after client upgrades.

## Verify and troubleshoot

```bash
systemctl --user status kompressor
journalctl --user -u kompressor -n 20 --no-pager | grep -i intercepting
ss -ltnp '( sport = :8817 or sport = :8840 )'
```

Both ports should be listening. A no-credential request verifies TLS trust and
the route without making a paid model call:

```bash
curl --silent --show-error --max-time 30 --noproxy '' \
  --proxy http://127.0.0.1:8840 \
  --cacert "$HOME/.local/share/kompressor/ca/ca.pem" \
  https://api.anthropic.com/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  --data '{"model":"claude-sonnet-4-6","max_tokens":1,"messages":[{"role":"user","content":"test"}]}' \
  -w '\nHTTP %{http_code}\n'
```

An Anthropic authentication error with HTTP **401** is expected here because
no credential was supplied. This proves connectivity, not subscription login,
compression, or retrieval. Verify those with a real Code session and Kompressor
statistics; join `optimization_events` and `proxy_requests` by `request_id`.

| Symptom | Check |
| --- | --- |
| Failed connection or repeated reconnects | `HTTPS_PROXY` must name the `[proxy.intercept] listen` port, and Kompressor must be running. |
| Address already in use | Something else holds the `listen` port. Find it with `ss -ltnp \| grep 8840`, or give the listener another port. |
| `TLS handshake failed` in the log | Two causes, indistinguishable from the server side. Check `alpn` contains `h2` first — Desktop offers only that — then `NODE_EXTRA_CA_CERTS`. The log line reports what was advertised. |
| Mid-session 404 from the proxy | No `/api` mount; a session probes `/api/hello` and signs in through `/api/oauth/*`. |
| Anthropic traffic reaching `api.openai.com` or `chatgpt.com` | Two mounts claim `/v1`, or this mount was inserted above a `[proxy.upstreams.subscription]` block and adopted it. |
| `intercept is listening with no intercept_hosts` | `intercept_hosts` is empty, so every connection is tunnelled. A working listener that compresses nothing. |
| Claude works but old tool results stay full | Check `CLAUDE_CODE_TETHER_LIVE=0`, restart Desktop, and use a fresh session. |
| Requests still bypass the listener | Check other environment/settings overrides, lowercase proxy variables, and `NO_PROXY` bypasses. |

For retrieval validation, prepare fresh fixture files before starting the
session, attach no files, and read `warmup_1.py`, `warmup_2.py`, then `ledger.py`
in separate tool calls. Follow with `wc -l warmup_1.py` before asking for the
hidden string in `ledger.py`'s `reconcile_17`. Use a fresh planted string and do
not reread the file: the model must retrieve its elided body. Full attached
copies or an answer already present in the conversation invalidate this test.

What has been verified of this path: a request reaches `api.anthropic.com`
through the listener over HTTP/2, with a certificate minted for the SNI and
verified by the client against the generated CA. That is connectivity and
routing.

Compression and retrieval figures through this listener have **not** been
re-measured; neither has OAuth refresh, nor behaviour after a client upgrade.
Treat the procedure above as the way to establish them, not as something
already established.

## Stop or undo the integration

Restore the backed-up Claude settings, or remove the three added environment
entries while preserving unrelated settings, then restart Desktop. Removing
`HTTPS_PROXY` while the listener is gone matters most: a client pointed at a
proxy that is not there fails every request.

Then take the listener out by removing `[proxy.intercept]` — or just its
`listen` — and restarting Kompressor. The rest of Kompressor keeps serving
other clients.

Finally, remove the certificate authority from any trust store it was installed
in, and delete `ca_path`. A local CA that outlives the experiment that needed it
is the part of this worth being tidy about; the fingerprint in the startup log
identifies the right one.

## Regular Chat is separate

This integration covers the Code tab's Anthropic Messages calls. Hosted Claude
Chat uses different endpoints, which this listener does not intercept. An MCP
registration can expose retrieval tools to Chat, but does not intercept its
inference requests or automatically compress server-held conversation history.
