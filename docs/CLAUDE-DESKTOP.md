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

Earlier versions of this document used mitmproxy and a Python addon for the
TLS side. That is no longer needed: `[proxy.intercept]` does the same work
inside the binary. [Migrating from the Python adapter](#migrating-from-the-python-adapter)
has the steps.

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
path_prefix = "/v1"
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

`[proxy.intercept]` is the listener that replaces the external TLS adapter. It
is off unless `listen` names an address, so an upgrade never starts one by
surprise.

The mounts are `/v1` and `/api`, not `/anthropic`. An intercepted request keeps
the path the client sent — the adapter had to rewrite to another port because
it was forwarding over HTTP to a separate process, and nothing in process needs
that indirection. `/api` is required as well as `/v1`: a Code session probes
`/api/hello` and signs in through `/api/oauth/*`, and without a mount for them
it takes a 404 from the proxy mid-session.

`alpn` must include `h2`. Desktop's client offers **only** `h2` for
`api.anthropic.com`; a listener advertising just `http/1.1` answers
`no_application_protocol`, the client hangs up mid-handshake, and the log shows
a handshake EOF that looks exactly like an untrusted certificate. It is not
one — `openssl s_client -alpn h2` against such a listener reports alert 120
with `Verify return code: 0 (ok)`.

> **If you already mount `openai` at `/v1`**, move it to `/openai` with
> `strip_prefix = true` first. Two mounts on `/v1` load without complaint and
> the first one wins, which sends Anthropic traffic to `api.openai.com`.
> Equally, put this mount **after** any `[proxy.upstreams.subscription]` block:
> TOML attaches that table to the most recent `[[proxy.upstreams]]`, so a mount
> inserted above one adopts it and forwards subscription traffic to the wrong
> provider.

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
| Address already in use | The old mitmproxy adapter is still bound to 8840. Stop it, or give the listener another port. |
| `TLS handshake failed` in the log | Two causes, indistinguishable from the server side. Check `alpn` contains `h2` first — Desktop offers only that — then `NODE_EXTRA_CA_CERTS`. The log line reports what was advertised. |
| Mid-session 404 from the proxy | No `/api` mount; a session probes `/api/hello` and signs in through `/api/oauth/*`. |
| Anthropic traffic reaching `api.openai.com` or `chatgpt.com` | Two mounts claim `/v1`, or this mount was inserted above a `[proxy.upstreams.subscription]` block and adopted it. |
| `intercept is listening with no intercept_hosts` | `intercept_hosts` is empty, so every connection is tunnelled. A working listener that compresses nothing. |
| Claude works but old tool results stay full | Check `CLAUDE_CODE_TETHER_LIVE=0`, restart Desktop, and use a fresh session. |
| Requests still bypass the adapter | Check other environment/settings overrides, lowercase proxy variables, and `NO_PROXY` bypasses. |

For retrieval validation, prepare fresh fixture files before starting the
session, attach no files, and read `warmup_1.py`, `warmup_2.py`, then `ledger.py`
in separate tool calls. Follow with `wc -l warmup_1.py` before asking for the
hidden string in `ledger.py`'s `reconcile_17`. Use a fresh planted string and do
not reread the file: the model must retrieve its elided body. Full attached
copies or an answer already present in the conversation invalidate this test.

A controlled test through the TLS adapter using Desktop's bundled 2.1.295
binary and saved user settings removed **15,303 tokens** on the final request,
performed **one retrieval round**, and returned the correct fresh string.
That request recorded 45,803 cache-read tokens summed across retrieval rounds.
A real GUI session must also be checked after changing settings. OAuth refresh
and behaviour after future client upgrades are not established by this test.

## Migrating from the Python adapter

If mitmproxy is already serving this integration, the switch is six steps. The
`HTTPS_PROXY` and `CLAUDE_CODE_TETHER_LIVE` settings do not change; only the
thing listening on the port and the CA behind it do.

**1. Stop the adapter and keep it stopped.** It holds port 8840, which the
listener is about to want.

```bash
systemctl --user disable --now kompressor-claude-tls
```

**2. Check the port is actually free.** A manually launched `mitmdump` is not
covered by the unit above.

```bash
ss -ltnp | grep 8840 || echo "8840 is free"
```

**3. Add `[proxy.intercept]` to the configuration** as in step 1, then restart
Kompressor and confirm it came up intercepting rather than merely listening.

```bash
systemctl --user restart kompressor
journalctl --user -u kompressor -n 20 --no-pager | grep -iE 'intercepting|certificate authority'
```

**4. Point the trust at the new CA.** This is the one settings change. In
`~/.claude/settings.json`, replace the `NODE_EXTRA_CA_CERTS` value with
`ca_path/ca.pem`; leave `HTTPS_PROXY` and `CLAUDE_CODE_TETHER_LIVE` alone.

```bash
grep -o '"NODE_EXTRA_CA_CERTS": "[^"]*"' ~/.claude/settings.json
```

**5. Prove the route before involving Desktop**, which costs nothing and fails
faster than a session does. HTTP **401** is the expected answer: it is
Anthropic rejecting a request with no credential, which means the whole path
worked.

```bash
curl --silent --show-error --max-time 30 --noproxy '' \
  --proxy http://127.0.0.1:8840 \
  --cacert "$HOME/.local/share/kompressor/ca/ca.pem" \
  https://api.anthropic.com/v1/messages \
  -w '\nHTTP %{http_code}\n'
```

**6. Fully quit and reopen Desktop, then start a new Code session.** A running
session keeps the environment it was spawned with, so a reopened tab is not
enough.

Then check that traffic is arriving, by request rather than by summary:

```bash
sqlite3 ~/.local/share/kompressor/stats.db \
  'select datetime(time_ms/1000,"unixepoch","localtime"), api_family, upstream, status
   from proxy_requests order by id desc limit 10;'
```

### Rolling back

Nothing is destroyed by trying this: the mitmproxy configuration directory and
its CA stay where they were. To go back, stop Kompressor, restore the previous
`NODE_EXTRA_CA_CERTS` value, and `systemctl --user enable --now
kompressor-claude-tls`. Only one of the two may hold the port at a time.

### Removing mitmproxy once it is no longer needed

Leave this until the built-in listener has carried a few real sessions.

```bash
systemctl --user disable --now kompressor-claude-tls
rm ~/.config/systemd/user/kompressor-claude-tls.service
systemctl --user daemon-reload
uv tool uninstall mitmproxy
```

The adapter's own CA is still trusted by whatever you installed it in. Remove
it from that trust store, then delete
`~/.local/share/kompressor/claude-tls/`, which holds both the addon and the
adapter's CA private key.

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
Chat uses different endpoints and has no Kompressor wire adapter. An MCP
registration can expose retrieval tools to Chat, but does not intercept its
inference requests or automatically compress server-held conversation history.
