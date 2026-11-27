# Gemini, Vertex AI and Bedrock

The proxy compresses five request shapes beyond the three direct APIs. Each is
recognised from the path the upstream will see, under any mount prefix, like
the direct ones. Design and acceptance criteria:
[design/HEADROOM-PORT.md](design/HEADROOM-PORT.md), Tier 5.

| Platform | Path | Body walked as | Model from |
|---|---|---|---|
| Google AI (Gemini API) | `/v1beta/models/{m}:generateContent`, `:streamGenerateContent` | Gemini | path |
| Vertex AI, Gemini | `…/projects/{p}/locations/{l}/publishers/google/models/{m}:generateContent` (and stream) | Gemini | path |
| Vertex AI, Claude | `…/publishers/anthropic/models/{m}:rawPredict`, `:streamRawPredict` | Anthropic Messages | path |
| Bedrock, Claude | `/model/{id}/invoke`, `/invoke-with-response-stream` | Anthropic Messages | path |
| Bedrock Converse | `/model/{id}/converse`, `/converse-stream` | Converse | path |

`:countTokens`, `:embedContent`, other Vertex publishers and every other
Bedrock action are forwarded untouched.

## What is compressed

- **Gemini**: `text` parts of `systemInstruction` and `contents`, and the
  string fields `output`, `content`, `result`, `text`, `stdout` of a
  `functionResponse.response` — which is where a Gemini agent's tool output
  is. A part marked `thought` or carrying a `thoughtSignature` is never
  rewritten; `inlineData`, `fileData` and `functionCall` are left as they are.
- **Claude on Vertex and Bedrock**: exactly what the direct Messages API gets,
  `cache_control` breakpoints included.
- **Converse**: `text` blocks and the `text` entries of a `toolResult`. A
  `cachePoint` freezes what precedes it, as `cache_control` does.

## What differs from the direct APIs

- **No in-band retrieve tool on Gemini, Converse or a Bedrock stream.** The
  proxy does not read those responses (Bedrock streams are binary AWS
  event-stream frames), so it could not answer the call. Handles still resolve
  through the MCP tools (`kompressor mcp serve`, or `wrap`'s session offer).
  Claude on Vertex streams Anthropic SSE and gets the loop as usual, as does a
  non-streamed Bedrock `invoke`.
- **No keep-warm and no `tools_defer`** off the direct Anthropic API: a ping
  re-sends a request whose token or signature was made for another body, and
  the clouds refuse or gate the tool-search shape.
- **Usage**: Gemini's `usageMetadata` (`promptTokenCount`,
  `cachedContentTokenCount`, `candidatesTokenCount`) and Converse's `usage`
  are recorded. A Bedrock event-stream's usage is inside binary frames and is
  not.
- **Settings**: `[proxy.providers.gemini]` and `[proxy.providers.bedrock]`
  apply to Gemini and Converse traffic; Claude on either cloud takes
  `[proxy.providers.anthropic]`.

## Authentication

| Platform | Client sends | The proxy |
|---|---|---|
| Google AI | `x-goog-api-key` or `?key=` | forwards both untouched |
| Vertex AI | `Authorization: Bearer <OAuth token>` | forwards it |
| Bedrock, API key | `Authorization: Bearer <key>` | forwards it |
| Bedrock, IAM | a SigV4 signature | **replaces it** when the mount has `aws_sigv4`; see below |

A SigV4 signature covers the body and the host, so it cannot survive a hop
through `127.0.0.1` or a compressed body. A mount with `aws_sigv4` has the
client's `authorization` and `x-amz-*` headers removed and every send signed
over the bytes that go out, with `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
and `AWS_SESSION_TOKEN` from the proxy's environment (export them from a
profile with `aws configure export-credentials --format env`). Without those
variables the client's headers go out and AWS decides.

## Configuration

```toml
[[proxy.upstreams]]
name = "gemini"
path_prefix = "/v1beta"
base_url = "https://generativelanguage.googleapis.com"

[[proxy.upstreams]]
name = "vertex"
path_prefix = "/vertex"
strip_prefix = true
base_url = "https://us-east5-aiplatform.googleapis.com"

[[proxy.upstreams]]
name = "bedrock"
path_prefix = "/model"
base_url = "https://bedrock-runtime.us-west-2.amazonaws.com"
aws_sigv4 = { region = "us-west-2", service = "bedrock" }
```

## Pointing clients at it

Claude Code on Bedrock, with the proxy signing:

```bash
export CLAUDE_CODE_USE_BEDROCK=1
export ANTHROPIC_BEDROCK_BASE_URL=http://127.0.0.1:8817
export CLAUDE_CODE_SKIP_BEDROCK_AUTH=1    # the proxy signs; see above
claude
```

Claude Code on Vertex: `CLAUDE_CODE_USE_VERTEX=1` and
`ANTHROPIC_VERTEX_BASE_URL=http://127.0.0.1:8817/vertex/v1` (the client keeps
fetching its own Google token).

Gemini CLI and the Google GenAI SDKs read `GOOGLE_GEMINI_BASE_URL`:

```bash
kompressor wrap --env GOOGLE_GEMINI_BASE_URL=root -- gemini
```

with the `gemini` upstream above in the configuration `wrap` runs under
(`--config`), since `wrap` mounts only the providers its catalogue names.
