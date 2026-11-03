# The end-to-end suite

```bash
make e2e
```

Three containers on an `internal: true` network — a mock provider, a
`kompressor` built from this working tree, and a driver that exercises them and
asserts. The run needs no credentials, no provider and no internet; only the
image build reaches crates.io.

## What it checks that nothing else does

`cargo test` checks what the engine returned. `make bench` checks the engine's
ratio against a recorded baseline. `scripts/smoke.sh` checks that the assembled
binary serves. None of them checks **what actually left the machine**, and that
is the claim a deployment cares about.

So the mock provider records every request body, and the driver reads them back:

| | |
|---|---|
| **Proxy flow, per mode** | The provider received a smaller body than the client sent, in `aggressive`, `lossless` and `recoverable` alike — asserted on the recorded bytes, not on the engine's own report. |
| **Mode guarantees** | `recoverable` announces every elision and keeps handles for what it drops; `aggressive` announces and retains nothing; `lossless` does neither, because it drops nothing. Run on the same payload, so a change that blurs two modes together fails. |
| **The one elision without a handle** | The cross-message fold, checked separately: a run repeated across two messages is elided from the later one, with no handle, and the earlier message still carries it verbatim. That is what makes it recoverable without retention, and it is the only form in `recoverable` that is allowed to mint nothing. |
| **Never larger, and deterministic** | Over four payload shapes × three modes, through the HTTP surface. |
| **Refs and retrievals** | A minted handle resolves; a line range returns strictly less than the whole; a range that matches nothing is a hit with empty content **and a hint**; an unknown handle is a miss with a hint and no invented content. |
| **Subscription routing** | The same Responses body goes to `/backend-api/codex/responses` when the caller's credential reads as a ChatGPT plan and to `/v1/responses` when it reads as an API key — asserted on the path the provider recorded, plus that the rewrite left a path the compressor still recognises. |
| **The retrieve loop** | For OpenAI Chat, OpenAI Responses and Anthropic Messages, buffered and streamed. The client gets one answer, never sees `optscale_kompressor_retrieve`, and the provider saw a second round carrying the fragment. |
| **A stream the provider does not label** | The mock answers streamed Responses with **no `Content-Type`**, which is what chatgpt.com does. The relay has to recognise it anyway, or the whole response takes the buffered path and the retrieve loop cannot intercept anything. |
| **Keep-warm** | With keep-warm on for Anthropic at a 2 s interval (`kompressor.toml`), the provider receives pings — the forwarded request with only `max_tokens: 1` and `stream: false` changed, each with its own `.keepwarmN` request id — after a `tool_use` stop up to the cap, and after an `end_turn` while a background task runs up to the lower background cap; none after an ordinary `end_turn` or once the task is reported, and none after the client's next request. The mock answers as a cached conversation only to requests carrying `KEEPWARM:`, so no other check can arm a timer. |
| **Compression efficiency** | Every corpus payload, classified by `/v1/detect`, compressed in all three modes, reported per type and gated on `runner/floors.json`. |

The streamed retrieve loop is the case worth having: a server-side loop that
runs only on buffered responses hands the tool call to a client that never
declared it, and that failure is invisible to every in-process test. It is not
hypothetical — it shipped. The mock had no streamed branch for Responses at
all, so nothing here noticed that a stream with no `Content-Type` skipped the
relay entirely; against real codex the model's retrieve call reached the
client, which rejected it, and the answer was wrong. Reverting the fix now
fails this suite on exactly that check.

## The efficiency table

```
  type                n      aggressive      lossless   recoverable
  build              44          72.81%        13.64%        72.98%
  listing            56          87.96%         0.00%        88.40%
  search             23          79.22%         0.00%        78.47%
  ...
  ALL               257          63.50%         6.48%        63.51%
```

`make bench` measures the same buckets by calling the engine directly. This
measures them through serialization, the walkers and the wire, so the two
should agree closely — and a gap between them lives in the plumbing rather than
in a compressor, which is worth knowing.

Floors live in [`runner/floors.json`](runner/floors.json), keyed `type@mode`
(plus `ALL@mode`). They are set below the measured value with room for tuning
noise: the point is to fail on a regression, not to pin a number that `make
bench` already pins more precisely. An empty `floors` object gates on nothing,
which is the right starting state for a new type.

## Layout

```
e2e/
  compose.yaml            three services on an internal network
  Dockerfile.kompressor   multi-stage: build from this tree, run on slim
  Dockerfile.harness      python:3.12-slim, standard library only
  kompressor.toml         the configuration under test
  provider/               the mock upstream, and its request recorder
  runner/                 the driver, and the floors it gates on
```

Both Python images install nothing. The provider and the driver are standard
library only, so the suite has no dependency that can drift underneath it and
the images build without an index.

`kompressor.toml` raises `engine.max_ref_bytes` well above the shipped default
on purpose. At 64 KiB a large fragment is elided with no handle, and this suite
asserts that recoverable mode retains what it drops; whether that default is
right is a separate argument, recorded in the README's gap list.

## Running it by hand

```bash
docker compose -f e2e/compose.yaml up --build     # leaves the stack up
docker compose -f e2e/compose.yaml run --rm runner # re-run just the checks
make e2e-down                                      # tear down, volumes included
```

`make e2e` does the first and last of those and propagates the driver's exit
code, so it is usable as a CI gate.
