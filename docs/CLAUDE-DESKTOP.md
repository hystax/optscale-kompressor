# Claude Desktop integration

Use a local TLS adapter to route the unmodified Claude Desktop **Code tab**
through Kompressor. Desktop retains its normal subscription login and OAuth
credentials; no separate API key is needed. Normal plan limits and any enabled
extra usage still apply. This guide covers Linux with systemd user services.

Validated 2026-10-21 with Desktop's bundled Claude Code **2.1.295**.

```text
Claude Desktop Code → TLS adapter 127.0.0.1:8840
                    → Kompressor 127.0.0.1:8817/anthropic
                    → https://api.anthropic.com
```

Both processes must run. Kompressor's HTTP reverse proxy does not implement
an HTTPS CONNECT proxy: **set `HTTPS_PROXY` to port 8840, not 8817**.

## Why Desktop needs an adapter

The CLI can use `kompressor wrap claude` or `ANTHROPIC_BASE_URL`. Signed-in
Desktop overrides that base URL, including values supplied through settings.
Desktop's third-party inference configuration instead uses separate gateway
credentials. The TLS adapter preserves the normal subscription authentication
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

[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/anthropic"
strip_prefix = true
base_url = "https://api.anthropic.com"
api_family = "anthropic"
forward_auth = true
```

`first_seen` opts into compressing history beyond the initially frozen prefix;
monitor cache usage as well as compression. Keep prompt caching enabled.

```bash
mkdir -p ~/.local/share/kompressor
kompressor serve --proxy --api --config ~/.config/kompressor/config.toml
```

Leave this running and use another terminal for the remaining steps. If an
existing Kompressor service already listens on 8817, use that instance instead
of launching a second one.

## 2. Install the TLS adapter

With `uv` installed, run:

```bash
uv tool install mitmproxy==12.2.3
```

From the root of this repository, install the supplied
[TLS addon](../scripts/claude-desktop/tls_adapter.py):

```bash
mkdir -p ~/.local/share/kompressor/claude-tls/mitmproxy
chmod 700 ~/.local/share/kompressor/claude-tls
chmod 700 ~/.local/share/kompressor/claude-tls/mitmproxy
cp scripts/claude-desktop/tls_adapter.py ~/.local/share/kompressor/claude-tls/

cat > ~/.local/share/kompressor/claude-tls/mitmproxy/config.yaml <<'EOF_CONFIG'
listen_host: 127.0.0.1
listen_port: 8840
allow_hosts:
  - '^api\.anthropic\.com:443$'
connection_strategy: lazy
upstream_cert: false
flow_detail: 0
termlog_verbosity: warn
EOF_CONFIG
```

The addon forwards only `POST /v1/messages` on `api.anthropic.com` to
`http://127.0.0.1:8817/anthropic/v1/messages`, preserving query parameters and
credentials. It decodes compressed uploads and streams responses without
buffering. Other API paths go directly upstream; other HTTPS hosts remain
CONNECT tunnels. It does not log request bodies or credentials.

## 3. Install and start the adapter service

The following assumes `uv` installed `mitmdump` in `~/.local/bin`. Check with
`command -v mitmdump` and adjust `ExecStart` if necessary.

```bash
mkdir -p ~/.config/systemd/user

cat > ~/.config/systemd/user/kompressor-claude-tls.service <<'EOF_SERVICE'
[Unit]
Description=Claude TLS adapter for local Kompressor
After=kompressor.service

[Service]
ExecStart=%h/.local/bin/mitmdump --set confdir=%h/.local/share/kompressor/claude-tls/mitmproxy -s %h/.local/share/kompressor/claude-tls/tls_adapter.py
Restart=on-failure
RestartSec=3
UMask=0077

[Install]
WantedBy=default.target
EOF_SERVICE

systemctl --user daemon-reload
systemctl --user enable --now kompressor-claude-tls
systemctl --user status kompressor-claude-tls
```

`enable --now` starts the adapter immediately and at future logins. To start it
only for the current login, use `systemctl --user start kompressor-claude-tls`.
`After=kompressor.service` orders startup if that service is also started; it
**does not start Kompressor**. A manually launched Kompressor works too.

For foreground operation instead of systemd, first stop any running adapter
service to free port 8840, then run:

```bash
~/.local/bin/mitmdump \
  --set confdir="$HOME/.local/share/kompressor/claude-tls/mitmproxy" \
  -s "$HOME/.local/share/kompressor/claude-tls/tls_adapter.py"
```

## 4. Configure Claude and certificate trust

On its first startup, mitmproxy automatically generates a local CA and reuses
it on later starts. The public certificate is:

```text
~/.local/share/kompressor/claude-tls/mitmproxy/mitmproxy-ca-cert.pem
```

Wait for the service to start and confirm that file exists. Use the certificate
from this adapter instance. The similarly named `mitmproxy-ca.pem` contains the
CA's private key and must not be distributed.

Back up `~/.claude/settings.json`, then merge these entries into its `env`
object, preserving other settings. Replace `/home/YOUR_USER` with your absolute
home path; JSON does not expand shell variables.

```json
{
  "env": {
    "HTTPS_PROXY": "http://127.0.0.1:8840",
    "NODE_EXTRA_CA_CERTS": "/home/YOUR_USER/.local/share/kompressor/claude-tls/mitmproxy/mitmproxy-ca-cert.pem",
    "CLAUDE_CODE_TETHER_LIVE": "0"
  }
}
```

Remove any previous `ANTHROPIC_BASE_URL` override from these Desktop settings.
`NODE_EXTRA_CA_CERTS` is required in this setup because Claude must trust the
adapter's locally signed certificate. It can be omitted only when that CA is
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
systemctl --user status kompressor-claude-tls
journalctl --user -u kompressor-claude-tls -n 20 --no-pager
ss -ltnp '( sport = :8817 or sport = :8840 )'
```

Both ports should be listening. A no-credential request verifies TLS trust and
the route without making a paid model call:

```bash
curl --silent --show-error --max-time 30 --noproxy '' \
  --proxy http://127.0.0.1:8840 \
  --cacert "$HOME/.local/share/kompressor/claude-tls/mitmproxy/mitmproxy-ca-cert.pem" \
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
| Failed connection or repeated reconnects | `HTTPS_PROXY` must use 8840; both adapter and Kompressor must run. |
| Address already in use | Do not run manual and service instances on the same port. |
| Certificate verification failure | Check the absolute CA path and that it belongs to the running adapter. |
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

## Stop or undo the integration

Restore the backed-up Claude settings, or remove the three added environment
entries while preserving unrelated settings, then restart Desktop. Stop the
adapter and disable its login startup with:

```bash
systemctl --user disable --now kompressor-claude-tls
```

Stopping the adapter while Claude still points to it causes connection failures.
Kompressor may continue serving other clients independently.

## Regular Chat is separate

This integration covers the Code tab's Anthropic Messages calls. Hosted Claude
Chat uses different endpoints and has no Kompressor wire adapter. An MCP
registration can expose retrieval tools to Chat, but does not intercept its
inference requests or automatically compress server-held conversation history.
