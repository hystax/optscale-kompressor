# Claude Desktop integration

Investigation date: 2026-10-20. Inspected Linux Desktop build: **2.42304.2**.

## Subscription mode is different from Claude Code CLI

The `claude` entry in `crates/kompressor-cli/src/wrap/agents.rs` launches the
CLI with `ANTHROPIC_BASE_URL` and `ENABLE_TOOL_SEARCH=true`. The `--attach`
implementation in `wrap/mod.rs` instead writes project-local
`.claude/settings.local.json` environment values.

Neither is sufficient for the signed-in Desktop Code tab. Desktop sets its
own API host when constructing the child process environment. It also filters
`ANTHROPIC_BASE_URL` out of its local environment editor. Anthropic documents
this precedence explicitly:

- [Desktop-controlled environment variables](https://code.claude.com/docs/en/desktop#environment-variables-desktop-sets-itself)
- [Desktop gateway configuration](https://code.claude.com/docs/en/llm-gateway-connect#desktop-app)

The supported gateway configuration uses separate gateway credentials. It is
not a setting that redirects the existing subscription session. Remote Control
also rejects a custom API base URL, so it is not a documented workaround:

- [Remote Control requirements](https://code.claude.com/docs/en/remote-control)

## Custom Code integration: live-validated on 2.42304.2

The installed `resources/app.asar` contains an environment builder in
`.vite/build/index.chunk-DOx99pQU.js`, named `pJ` in this build. Its output
includes:

```javascript
ANTHROPIC_BASE_URL: e.apiHost,
CLAUDE_CODE_OAUTH_TOKEN: e.oauthToken,
ANTHROPIC_API_KEY: "",
ANTHROPIC_AUTH_TOKEN: "",
```

An offline test evaluated the actual builder with synthetic inputs and a
candidate override. Only `ANTHROPIC_BASE_URL` changed for local Code sessions;
OAuth fields were preserved. The override was disabled for Cowork and
third-party inference sessions. The isolated experiment below then completed
live requests through it; see "Live validation".

A deployable integration should:

1. Use an isolated app copy or a reproducible package patch. Verify the app
   version, bundle hash and unique replacement target before patching.
2. Opt in through a dedicated launcher. Accept only the configured loopback
   proxy URL; do not change authentication, login or token refresh endpoints.
3. Change the local Code child process's API base URL after Desktop composes
   its environment. Preserve its OAuth handling. Keep tool search enabled.
4. Start a dedicated Kompressor instance before launching Desktop. Keep its
   statistics and reference stores separate from sweep databases.
5. Validate with a new Code session: proxy requests must appear, then a
   multi-turn file-reading test must produce live handles and a successful
   retrieval. A successful chat response alone does not prove proxy routing.
6. Test restart, token refresh, proxy failure, and restore of the original app.
   Refuse an unrecognized app update instead of applying a stale patch.

Kompressor already parses Anthropic Messages and recognizes Desktop's
`claude-cli/... (external, claude-desktop)` user agent as a subscription client.
The earlier Claude Code sweep exercises that Messages compression/retrieval
path, but does not validate the Desktop launcher.

The dedicated proxy needs an `anthropic` mount for `/v1/messages` forwarding
to `https://api.anthropic.com`, with `forward_auth=true`. Other `/v1` requests
(including model discovery) and `/api` requests should use passthrough mounts
to that same upstream. Put the specific Messages mount before `/v1`.

Use `engine.mode="recoverable"` and, if opting into tail compression,
`proxy.frozen_prefix="first_seen"`. Monitor cache usage as well as compression:
this is a deliberate change from the conservative `marker` default.
Use absolute paths for the stats and reference stores.

## Regular Chat requires separate investigation

The installed app explicitly tracks hosted
`chat_conversations/<id>/completion`, `completion2`, retry variants, and a
mobile completion service. These are not Anthropic Messages.

`crates/kompressor-wire/src/lib.rs` supports OpenAI Chat Completions, OpenAI
Responses and Anthropic Messages. There is no hosted Claude Chat adapter.
Pointing a mount at `claude.ai` or configuring an HTTPS proxy does not create
one; passthrough traffic is not compressed.

Before implementing a Chat adapter, capture a controlled, non-sensitive
conversation and determine:

- whether requests contain history or only a conversation reference and new input;
- which tool results or attachment contents are locally available to compress;
- whether the hosted service permits an appropriate retrieval tool;
- how streamed events, retries and stored conversation state interact.

History assembled only on the remote service cannot be rewritten by a local
request proxy. Compressing just newly submitted material is a different and
more limited feature. The request payload of regular Chat was **not captured**
in this investigation; full-history compression is therefore unproven.

An MCP registration can expose Kompressor tools to Chat, but does not intercept
its inference requests or automatically compress its conversation history.

## Isolated experiment

The version-pinned preparation script copies the installed Linux app, checks
the target bundle hash, patches only Code's API URL, and rebuilds ASAR offsets
and SHA-256 integrity metadata. It refuses to overwrite an experiment directory.

```bash
python3 scripts/claude-desktop-experiment/prepare.py /tmp/claude-kompressor-test
scripts/claude-desktop-experiment/launch.sh /tmp/claude-kompressor-test
```

The launcher uses separate XDG configuration, data and cache directories, starts
Kompressor on `127.0.0.1:8839`, and stops that proxy when the experiment app
exits. Sign into the separate profile. Existing Desktop windows and Code
sessions are not rerouted. The patch leaves Chat and Cowork unchanged.

`prepare.py` also writes the sweep's retrieval fixture (`fixture/warmup_1.py`,
`warmup_2.py`, `ledger.py`) with a fresh planted token in `expected-token.txt`.
Until the isolated profile has signed in, `launch.sh` routes `claude://` login
callbacks to it through `~/.local/share/applications/kompressor-claude-experiment.desktop`;
the original handler is restored on the first callback or on exit. The proxy
logs to `logs/proxy.log` with `kompressor_proxy=debug`, which names the upstream
mount of every request but not its body.

Start a new local Code session rooted at `fixture/` and paste `prompt.txt`
(copied from `scripts/claude-desktop-experiment/prompt.txt`) as one message. Its
shape matters:

- One read per turn. Reading all three files in one turn put them in the first
  substantial turn, which `frozen_prefix="first_seen"` leaves alone: zero
  savings, token read in the clear.
- A harmless turn after `cat ledger.py`. The proxy never compresses the newest
  tool result, so `ledger.py` is only folded once another turn follows. The
  sweep uses `rm ledger.py` there; Desktop's Code session declined to delete a
  file on pasted instructions and answered in the same request, so the prompt
  uses `wc -l` instead.
- A fresh session per attempt. A session that already printed the token can
  answer from its own reply.

Check progress, per-turn savings, cache usage and the reply with:

```bash
python3 scripts/claude-desktop-experiment/status.py /tmp/claude-kompressor-test "<final reply>"
```

`status.py` joins `optimization_events` to `proxy_requests` by `request_id`:
the proxy leaves `proxy_requests.compression_event_id` NULL even on compressed
Messages requests (observed for streaming and non-streaming), which is a stats
bug to fix separately.

Inspect `/tmp/claude-kompressor-test/state/stats.db` for actual proxy requests.
Starting the app or receiving a Claude reply is not a successful pairing unless
requests appear there. For a compression test, verify live optimization events
and successful retrievals as well. The app's normal installation is untouched;
rollback consists of quitting the experiment.

## Live validation

Run 2026-10-20, Desktop 2.42304.2, model `claude-opus-5-5`, subscription
sign-in in the isolated profile, `engine.mode="recoverable"`,
`frozen_prefix="first_seen"`.

- Routing: Code requests reached the proxy as `anthropic_messages` (streaming
  and non-streaming) and passthrough `/v1` and `/api` traffic, all HTTP 200.
  OAuth was untouched.
- Compression: each earlier `cat` result was folded once it stopped being the
  newest; the request answering the question carried all three files folded,
  15,309 tokens saved.
- Retrieval: that request made one retrieve round; the injected tool returned
  the `reconcile_17` fragment (1,118 characters) and the reply was the planted
  token. `status.py` reported `RETRIEVED` and `ANSWER CORRECT`.
- Cache: the folded history stayed byte-stable; the retrieving request read
  123,004 tokens from cache and wrote 1,276.

Not yet tested: app restart, OAuth token refresh, proxy failure mid-session
(the patched app has no fallback to the direct API host), and behaviour after
a Desktop update (prepare.py refuses an unrecognized build).
