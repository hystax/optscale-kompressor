#!/usr/bin/env python3
"""End-to-end checks against a running kompressor, from outside the process.

What this adds over `cargo test` and `scripts/smoke.sh`:

* **It asserts on the bytes the upstream received.** Every unit test in the
  workspace checks what the engine returned; none of them checks that the
  smaller body is what actually left the machine. The mock provider records
  each request and this reads them back.
* **It covers all three modes on the same payloads**, which is where the
  mode guarantees differ and where a change to one can quietly alter another.
* **It measures compression per detected type through the HTTP surface**, and
  fails on a floor. `make bench` measures the same thing by calling the engine
  directly; this measures it through serialization, the walkers and the wire,
  which is the number a deployment actually gets.
* **It runs the retrieve loop for real**, including the streamed relay, and
  checks the client never sees the tool call the proxy answered on its behalf.

Standard library only and no network beyond the compose bridge.
"""

import gzip
import itertools
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from collections import defaultdict

API = os.environ.get("KOMPRESSOR_API", "http://kompressor:8817")
PROXY = os.environ.get("KOMPRESSOR_PROXY", "http://kompressor:8817")
PROVIDER = os.environ.get("PROVIDER", "http://provider:8080")
CORPUS = os.environ.get("CORPUS", "/corpus/corpus.jsonl.gz")

MODES = ("aggressive", "lossless", "recoverable")
HANDLE = re.compile(r"ref:(k_[A-Za-z0-9_-]{12})")

# Markers the engine emits when it drops something. `has_marker` in the engine
# keys on the same literals; they are repeated rather than imported because a
# test that shares its subject's constants cannot catch them changing.
OMISSION_MARKERS = (" omitted", "[×", " chars]")

failures: list[str] = []
notes: list[str] = []


def check(ok: bool, what: str, detail: str = "") -> bool:
    if ok:
        print(f"  \033[32mok\033[0m   {what}")
    else:
        print(f"  \033[31mFAIL\033[0m {what}" + (f"\n         {detail}" if detail else ""))
        failures.append(what)
    return ok


def post(url: str, payload, headers: dict | None = None, raw: bytes | None = None):
    body = raw if raw is not None else json.dumps(payload).encode()
    request = urllib.request.Request(url, data=body, method="POST")
    request.add_header("content-type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), error.read()


def get(url: str):
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.status, response.read()


def wait_for(url: str, seconds: int = 120) -> None:
    deadline = time.time() + seconds
    last = ""
    while time.time() < deadline:
        try:
            status, _ = get(url)
            if status == 200:
                return
        except Exception as error:  # noqa: BLE001 - any failure means not ready
            last = repr(error)
        time.sleep(0.5)
    raise SystemExit(f"{url} never became ready: {last}")


def recorded() -> list[dict]:
    _, body = get(f"{PROVIDER}/_recorded")
    return json.loads(body)


def reset_provider() -> None:
    post(f"{PROVIDER}/_reset", {})


def log_payload(lines: int = 900, tag: str = "") -> str:
    """A redundant build log: the shape the engine is built for.

    `tag` makes the payload — and therefore the handle it compresses to, and
    the conversation the proxy identifies it as — unique to one scenario.

    Two scenarios sharing a payload are ONE conversation to the proxy, because
    identity is the conversation's opening text and these open with nothing
    else. That is not a quirk of the fixture: once a fragment has been fetched
    back in two separate requests the engine stops eliding it, so scenario
    three onward would find no handle to retrieve and the retrieve-loop checks
    would fail on a proxy that is working exactly as designed. Caught in CI by
    six checks at once.
    """
    head = f"2026-09-19T10:00:00Z INFO  scenario {tag}\n" if tag else ""
    return head + "\n".join(
        f"2026-09-19T10:{(i // 60) % 60:02d}:{i % 60:02d}Z INFO  worker[{i % 8}] "
        f"processing batch seq={i} items=128 elapsed_ms={(i * 37) % 90}"
        for i in range(lines)
    )


# ---------------------------------------------------------------------------


def surfaces_answer() -> None:
    print("\nsurfaces")
    for path in ("/healthz", "/readyz", "/metrics"):
        try:
            status, _ = get(f"{API}{path}")
        except Exception as error:  # noqa: BLE001
            status = repr(error)
        check(status == 200, f"GET {path}", f"got {status}")


def proxy_flow_in_every_mode() -> dict:
    """The whole path, per mode, asserted on what the upstream received."""
    print("\nproxy flow, per mode")
    handles_by_mode: dict[str, list[str]] = {}
    payload = log_payload()

    for mode in MODES:
        reset_provider()
        body = {
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": "summarise the log"},
                {"role": "user", "content": payload},
            ],
        }
        sent = len(json.dumps(body).encode())
        status, headers, _ = post(
            f"{PROXY}/v1/chat/completions", body, {"x-kompressor-mode": mode}
        )
        check(status == 200, f"[{mode}] proxy relays a completion", f"status {status}")
        check(
            headers.get("x-kompressor-mode") == mode,
            f"[{mode}] response says which mode handled it",
            f"got {headers.get('x-kompressor-mode')!r}",
        )

        seen = recorded()
        if not check(seen, f"[{mode}] the upstream was reached"):
            continue
        upstream = seen[0]["body"]
        got = seen[0]["bytes"]

        # The claim that matters for a lossy mode: the provider received less
        # than the client sent. Everything else is downstream of that.
        #
        # Lossless is exempt and must be: it drops nothing, so on a payload
        # with no reversible transform available it forwards the original —
        # and the retrieve tool the proxy appends can make the body *larger*.
        # Its guarantee is checked below instead, as the guarantee it makes.
        if mode != "lossless":
            check(
                got < sent,
                f"[{mode}] the upstream received a smaller body",
                f"{sent} sent, {got} received",
            )

        found = HANDLE.findall(upstream)
        marked = any(marker in upstream for marker in OMISSION_MARKERS)
        handles_by_mode[mode] = found

        if mode == "recoverable":
            check(marked, "[recoverable] the elision is announced")
            # "at least one", not "every": the cross-message fold elides
            # without minting anything, and `cross_turn_fold_needs_no_handle`
            # is where that form is checked. This payload is a single message,
            # so nothing here can take that path — but the label used to claim
            # more than the assertion, which is how the gap stayed invisible.
            check(bool(found), "[recoverable] what it dropped is behind a handle")
        elif mode == "aggressive":
            check(marked, "[aggressive] the elision is announced")
            check(
                not found,
                "[aggressive] and retains nothing",
                f"{len(found)} handle(s) leaked into an unrecoverable mode",
            )
        else:  # lossless
            check(
                not marked and not found,
                "[lossless] nothing is dropped and nothing is retained",
                "an omission marker or handle appeared in the one mode that promises neither",
            )
            # The size of the body is the wrong question for this mode; whether
            # the content survived is the right one. Both ends of the payload
            # have to still be there, because a compressor that kept a head and
            # dropped a tail would pass every other check here.
            lines = payload.splitlines()
            check(
                lines[0] in upstream and lines[-1] in upstream,
                "[lossless] the payload reaches the upstream intact",
                "the first or last line of the input did not survive",
            )

    return handles_by_mode


def the_control_arm_forwards_unchanged_and_says_so() -> None:
    """`off` is a real mode on the wire, not a missing one.

    This is the uncompressed baseline a cost comparison is measured against, and
    for most of this project's life it could not be run: `--mode off` was written
    into `[engine] mode`, which has three variants and no passthrough, so every
    control run died at `check-config` before spending anything. Thirty-odd
    compressed runs were collected against no baseline at all.

    Three claims, because a baseline that fails any of them is worse than none:
    the provider gets exactly what the client sent, the response says `off` so a
    run can be told apart from a compressed one afterwards, and the request is
    still *recorded* — a control arm that produces no telemetry is an expensive
    way to prove the proxy can forward bytes.
    """
    print("\ncontrol arm")
    reset_provider()
    payload = log_payload()
    body = {
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "summarise the log"},
            {"role": "user", "content": payload},
        ],
    }
    sent = json.dumps(body)
    status, headers, _ = post(
        f"{PROXY}/v1/chat/completions", body, {"x-kompressor-mode": "off"}
    )
    check(status == 200, "[off] the proxy relays a completion", f"status {status}")
    check(
        headers.get("x-kompressor-mode") == "off",
        "[off] the response says the run was not compressed",
        f"got {headers.get('x-kompressor-mode')!r}",
    )

    seen = recorded()
    if not check(seen, "[off] the upstream was reached"):
        return
    check(
        seen[0]["body"] == sent,
        "[off] the provider received exactly what the client sent",
        f"{len(seen[0]['body'])} bytes against {len(sent)}",
    )
    check(
        "ref:k_" not in seen[0]["body"],
        "[off] and nothing was elided behind a handle",
    )


def subscription_routing() -> None:
    """The same request goes to a different place depending on who is asking.

    Codex under a ChatGPT plan sends an ordinary Responses body to
    `chatgpt.com/backend-api/codex/responses`; under an API key it sends the
    same body to `api.openai.com/v1/responses`. Only the credential
    distinguishes them, so the proxy has to route on it — and forwarding a
    subscriber to the API-key host returns 404, which Codex reads as an invalid
    session and stops using the subscription at all.

    Both halves are checked on the path the provider recorded, because the
    failure this guards against is the proxy believing it routed correctly.
    """
    print("\nsubscription routing")
    payload = log_payload()
    body = {
        "model": "gpt-5.5",
        "input": [
            {"role": "user", "content": "summarise the log"},
            {"role": "user", "content": payload},
        ],
    }
    sent = len(json.dumps(body).encode())

    # A subscription caller: a known CLI carrying something that is not a
    # per-token key. Both signals are needed; either alone classifies as payg.
    reset_provider()
    status, _, _ = post(
        f"{PROXY}/v1/responses",
        body,
        {
            "user-agent": "codex_exec/0.155.1 (Linux; x86_64)",
            "authorization": "Bearer chatgpt-plan-token-not-an-api-key",
        },
    )
    check(status == 200, "a subscription caller is relayed", f"status {status}")
    seen = recorded()
    if check(seen, "and reached the upstream"):
        check(
            seen[0]["path"] == "/backend-api/codex/responses",
            "on the subscription path",
            f"recorded {seen[0]['path']!r}",
        )
        # The rewrite has to leave a path the family detector still knows, or
        # the body is forwarded whole and nothing reports it.
        check(
            seen[0]["bytes"] < sent,
            "and the body was still compressed after the rewrite",
            f"{sent} sent, {seen[0]['bytes']} received",
        )

    # The same request under an API key must not be rewritten.
    reset_provider()
    status, _, _ = post(
        f"{PROXY}/v1/responses",
        body,
        {
            "user-agent": "codex_exec/0.155.1 (Linux; x86_64)",
            "authorization": "Bearer sk-an-ordinary-api-key",
        },
    )
    check(status == 200, "an API-key caller is relayed", f"status {status}")
    seen = recorded()
    if check(seen, "and reached the upstream"):
        check(
            seen[0]["path"] == "/v1/responses",
            "on the ordinary path, not the subscription one",
            f"recorded {seen[0]['path']!r}",
        )


def never_larger_and_deterministic() -> None:
    """Two invariants the README puts above ratio, checked over the wire."""
    print("\ninvariants")
    payloads = {
        "log": log_payload(200),
        "prose": "Some ordinary prose that no compressor should claim. " * 40,
        "tiny": "short",
        "json": json.dumps([{"id": i, "name": f"row-{i}", "ok": True} for i in range(200)]),
    }
    for mode in MODES:
        for name, text in payloads.items():
            first = compress_raw(text, mode)
            second = compress_raw(text, mode)
            check(
                len(first) <= len(text),
                f"[{mode}] {name}: never larger than its input",
                f"{len(text)} -> {len(first)}",
            )
            check(
                first == second,
                f"[{mode}] {name}: the same input gives the same bytes",
            )


def compress_raw(content: str, mode: str) -> str:
    status, _, body = post(
        f"{API}/v1/compress/raw", {"content": content, "options": {"mode": mode}}
    )
    if status != 200:
        raise SystemExit(f"/v1/compress/raw returned {status}: {body[:200]!r}")
    return json.loads(body)["content"]


def detect(content: str) -> str:
    status, _, body = post(f"{API}/v1/detect", {"content": content})
    if status != 200:
        return "unknown"
    return json.loads(body).get("content_type", "unknown")


def every_handle_resolves(payload: str, label: str) -> None:
    """Every handle the view offers comes back with content.

    This is what `recoverable` promises and it is narrower than it sounds. The
    mode says every *elision* keeps a handle; it does not say the input can be
    reconstructed line by line, and a check that assumed so fails honestly on
    the log compressor — template normalisation rewrites a varying value into a
    placeholder rather than stashing the original, which is accounted for by
    the marker without being retained. "Accounted for" and "retained verbatim"
    are different promises and only the first is made.

    What would be a real defect is a handle in the view that resolves to
    nothing: the model is told it can ask, asks, and gets an empty answer. That
    is what this catches, over every handle rather than the first one.
    """
    view = compress_raw(payload, "recoverable")
    handles = sorted(set(HANDLE.findall(view)))
    if not check(handles, f"[{label}] recoverable mode minted handles"):
        return
    dangling, empty = [], []
    for handle in handles:
        status, _, body = post(f"{API}/v1/retrieve", {"ref": handle})
        got = json.loads(body) if status == 200 else {}
        if status != 200 or got.get("found") is not True:
            dangling.append(handle)
        elif not (got.get("content") or "").strip():
            empty.append(handle)
    check(
        not dangling,
        f"[{label}] all {len(handles)} handles resolve",
        f"dangling: {dangling[:3]}",
    )
    check(
        not empty,
        f"[{label}] and none of them resolves to nothing",
        f"empty: {empty[:3]}",
    )


def handles_are_never_dangling() -> None:
    """`every_handle_resolves` over the shapes that mint the most handles."""
    print("\nhandles")
    every_handle_resolves(search_payload(), "search")
    every_handle_resolves(log_payload(600), "log")


def search_payload(files: int = 40, per_file: int = 12) -> str:
    """grep -rn output: many files, each with several hits.

    Built rather than sampled from the corpus because this exercises the
    file-grouping and ranking path, which needs enough files to rank.
    """
    lines = []
    for f in range(files):
        for h in range(per_file):
            lines.append(
                f"crates/pkg{f:02d}/src/module_{f:02d}.rs:{100 + h * 7}:"
                f"    pub fn handler_{f:02d}_{h:02d}(request: &Request) -> Result<Response>"
            )
    return "\n".join(lines) + "\n"


def refs_and_retrievals(handles: list[str]) -> None:
    """A handle is only worth minting if it comes back."""
    print("\nrefs and retrievals")
    if not check(handles, "recoverable mode minted at least one handle"):
        return
    handle = handles[0]

    status, _, body = post(f"{API}/v1/retrieve", {"ref": handle})
    got = json.loads(body) if status == 200 else {}
    check(status == 200 and got.get("found") is True, f"{handle} resolves", f"status {status}")
    whole = got.get("content") or ""
    check(bool(whole.strip()), "and returns the fragment, not an empty hit")

    # A narrowed retrieval is the cheap path and the one a model is told to
    # prefer, so it has to actually narrow.
    status, _, body = post(
        f"{API}/v1/retrieve", {"ref": handle, "range": {"lines": {"start": 1, "end": 3}}}
    )
    ranged = json.loads(body) if status == 200 else {}
    check(
        ranged.get("found") is True and 0 < len(ranged.get("content") or "") < len(whole),
        "a line range returns less than the whole fragment",
        f"{len(ranged.get('content') or '')} of {len(whole)} chars",
    )

    # The regression this suite exists to hold: a range that selects nothing is
    # a *hit* with empty content, and used to arrive with no explanation at all,
    # so a model spent its next round learning nothing.
    status, _, body = post(
        f"{API}/v1/retrieve",
        {"ref": handle, "range": {"grep": {"pattern": "nothing-matches-this-xyzzy"}}},
    )
    empty = json.loads(body) if status == 200 else {}
    check(
        empty.get("found") is True and (empty.get("content") or "") == "",
        "a range that matches nothing is still a hit with empty content",
    )
    check(
        bool(empty.get("hint")),
        "and says how the range missed",
        f"no hint in {sorted(empty)}",
    )

    # A handle that was never minted must say so rather than invent anything.
    status, _, body = post(f"{API}/v1/retrieve", {"ref": "k_AAAAAAAAAAAA"})
    miss = json.loads(body) if status == 200 else {}
    check(miss.get("found") is False, "an unknown handle is a miss")
    check(bool(miss.get("hint")), "and the miss tells the model to regenerate")
    check("content" not in miss or not miss.get("content"), "and invents nothing")


def retrieve_loop(
    family: str, path: str, body: dict, streamed: bool, narrate: bool = False
) -> None:
    """The proxy answers its own tool call and hides the round from the client."""
    reset_provider()
    label = f"{family}{' streamed' if streamed else ''}{' narrated' if narrate else ''}"
    extra = {"x-kompressor-mode": "recoverable"}
    if narrate:
        extra["x-e2e-narrate"] = "1"
    status, headers, response = post(f"{PROXY}{path}", body, extra)
    check(status == 200, f"[{label}] the client gets one answer", f"status {status}")

    text = response.decode("utf-8", "replace")
    check(
        "optscale_kompressor_retrieve" not in text,
        f"[{label}] and never sees the tool it did not declare",
    )

    seen = recorded()
    check(
        len(seen) >= 2,
        f"[{label}] the proxy went back to the provider with the fragment",
        f"{len(seen)} upstream request(s); the loop did not run",
    )
    if len(seen) >= 2:
        check(
            any(
                marker in seen[1]["body"]
                for marker in ('"role":"tool"', '"function_call_output"', '"tool_result"')
            ),
            f"[{label}] the second round carries the retrieved fragment",
        )
    if not streamed:
        rounds = headers.get("x-kompressor-retrieve-rounds")
        check(rounds not in (None, "0"), f"[{label}] the round is reported in a header", f"got {rounds!r}")


def retrieve_loops() -> None:
    print("\nretrieve loop")
    # One payload per scenario. A shared one makes every scenario the same
    # conversation, and the second retrieval of the same handle then promotes
    # it out of being elided at all. See `log_payload`.
    tags = iter(f"r{n}" for n in itertools.count(1))
    scenario = lambda: log_payload(tag=next(tags))  # noqa: E731
    retrieve_loop(
        "openai_chat",
        "/v1/chat/completions",
        {"model": "gpt-4", "messages": [{"role": "user", "content": scenario()}]},
        streamed=False,
    )
    retrieve_loop(
        "anthropic",
        "/v1/messages",
        {
            "model": "claude-x",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": scenario()}],
        },
        streamed=False,
    )
    retrieve_loop(
        "openai_responses",
        "/v1/responses",
        {"model": "gpt-4", "input": [{"role": "user", "content": [{"type": "input_text", "text": scenario()}]}]},
        streamed=False,
    )
    retrieve_loop(
        "openai_chat",
        "/v1/chat/completions",
        {"model": "gpt-4", "stream": True, "messages": [{"role": "user", "content": scenario()}]},
        streamed=True,
    )
    retrieve_loop(
        "anthropic",
        "/v1/messages",
        {
            "model": "claude-x",
            "max_tokens": 64,
            "stream": True,
            "messages": [{"role": "user", "content": scenario()}],
        },
        streamed=True,
    )
    # The second case that shipped broken, and the more expensive one. A
    # reasoning model says a sentence before it calls a tool; the relay used to
    # read that sentence as proof the turn belonged to the client, hand the
    # buffer over and relay everything after it untouched — so the retrieve
    # calls reached a client that answered `unsupported call` and the fragment
    # was lost. Measured against codex on gpt-5.5: thirteen calls leaked in one
    # session, nothing fetched, and the bug hunt read FAIL. The same commit on
    # gpt-6-sol, which calls without narrating, retrieved twenty and passed —
    # which is how a defect this total hid behind a green suite.
    retrieve_loop(
        "openai_responses",
        "/v1/responses",
        {
            "model": "gpt-4",
            "stream": True,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": scenario()}]}],
        },
        streamed=True,
        narrate=True,
    )
    # The case that shipped broken. The provider answers this one with **no
    # Content-Type at all**, which is what chatgpt.com does: the proxy used to
    # require the header to recognise a stream, so the whole response took the
    # buffered path, the relay never ran, and the model's `optscale_kompressor_retrieve`
    # call reached the client — which rejected it and answered without the
    # fragment. Measured against real codex before the fix: a wrong answer.
    retrieve_loop(
        "openai_responses",
        "/v1/responses",
        {
            "model": "gpt-4",
            "stream": True,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": scenario()}]}],
        },
        streamed=True,
    )


def a_stream_the_provider_does_not_label_is_still_relayed() -> None:
    """The provider sends the Responses stream with no Content-Type.

    `retrieve_loops` covers what that costs when a retrieve call is in flight.
    This covers the plainer half: a streamed answer with nothing to retrieve
    still has to reach the client, and reach it as a stream rather than as one
    buffered lump delivered when the model has finished.
    """
    print("\nan unlabelled stream")
    reset_provider()
    body = {
        "model": "gpt-4",
        "stream": True,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
    }
    status, _, raw = post(f"{PROXY}/v1/responses", body)
    text = raw.decode("utf-8", "replace") if isinstance(raw, bytes) else str(raw)
    check(status == 200, "an unlabelled stream is relayed", f"status {status}")
    check(
        "response.completed" in text,
        "and the client receives the frames",
        "the body carried no Responses events",
    )
    check("optscale_kompressor_retrieve" not in text, "and never sees our tool")


def generated_text_folds_on_its_template_not_only_on_exact_repeats() -> None:
    """A plain-text fold used to match lines byte for byte and nothing else.

    Right for prose, wrong for generated text: a markdown guide of near-identical
    fenced blocks came back at 7.15%. Both identities are computed now and the
    shorter ships, with exact winning ties so prose is untouched.
    """
    print("\ngenerated text")
    guide = "".join(
        "### Step {i}\n\nRun the helper and check the result.\n\n```python\n".format(i=i)
        + "".join(
            "    value_{k} = transform(records, {k}, offset={o})\n".format(k=k, o=i * k)
            for k in range(26)
        )
        + "```\n\n"
        for i in range(60)
    )
    out = compress_raw(guide, "recoverable")
    removed = 100.0 * (len(guide) - len(out)) / len(guide)
    check(
        removed > 60.0,
        "generated text folds on its template",
        f"only {removed:.2f}% removed — the fold is matching lines exactly",
    )
    check("ref:k_" in out, "and the values it normalised stay fetchable", out[-200:])

    # Prose whose lines do not repeat is not folded into something shorter and
    # wronger. Exact wins ties, so this is the property that ordering buys.
    prose = "".join(
        "The collector keeps a bounded queue and drops the oldest event number {i} "
        "when the writer falls behind, which must never become backpressure.\n".format(i=i)
        for i in range(200)
    )
    folded = compress_raw(prose, "recoverable")
    check(
        len(folded) <= len(prose),
        "prose is never made larger",
        f"{len(prose)} -> {len(folded)}",
    )


def a_generated_config_is_folded_rather_than_barely_touched() -> None:
    """The config route claims a payload and can remove almost nothing.

    Its transforms elide values one key at a time, which is right for a
    hand-written file and wrong for the generated kind an agent cats: 600
    repeating TOML blocks came back at 0.85%. Both readings are computed now
    and the shorter ships.
    """
    print("\ngenerated config")
    toml = "".join(
        "[worker.pool_{i}]\nsize = {a}\ntimeout_ms = {b}\nretries = {c}\n"
        'endpoint = "https://svc-{d}.internal:8443"\n\n'.format(
            i=i, a=i % 16 + 1, b=1000 + i % 900, c=i % 5, d=i % 30
        )
        for i in range(400)
    )
    check(detect(toml) == "config", "the payload is detected as config", detect(toml))
    out = compress_raw(toml, "recoverable")
    removed = 100.0 * (len(toml) - len(out)) / len(toml)
    check(
        removed > 60.0,
        "a generated config is folded rather than barely touched",
        f"only {removed:.2f}% removed",
    )
    check(
        "ref:k_" in out,
        "and what the fold removed stays fetchable",
        out[-200:],
    )


def json_behind_a_harness_frame_is_still_json() -> None:
    """`try_json` gives up on the first byte, and a framed payload never opens `[`.

    Every harness stamps a frame, so a valid document was being called `text`.
    The compressed bytes rarely move — the mixed segmenter finds the span
    either way — but a detector that answers `text` for a JSON array sends
    whoever is reading it somewhere wrong.
    """
    print("\njson behind a frame")
    body = json.dumps(
        [{"id": i, "name": f"item-{i}", "team": f"team-{i % 9}"} for i in range(300)],
        indent=2,
    )
    framed = (
        "Chunk ID: e2e002\n"
        "Wall time: 0.0000 seconds\n"
        "Process exited with code 0\n"
        "Original token count: 7309\n"
        "Output:\n" + body
    )
    check(detect(body) == "json_array", "the bare document is json", detect(body))
    check(
        detect(framed) == "json_array",
        "and so is the same document behind a harness frame",
        detect(framed),
    )
    out = compress_raw(framed, "recoverable")
    check(
        out.startswith("Chunk ID: e2e002\n"),
        "the frame still comes back verbatim",
        out[:40],
    )
    check(len(out) < len(framed), "and the body is compressed", f"{len(framed)} -> {len(out)}")


def html_is_compressed_rather_than_waved_through() -> None:
    """HTML had no compressor: `strategy_for` had no arm for it.

    Detection was precise and nothing acted on it, so every HTML payload came
    back whole — 40 KB at 0.00% on a live codex session. It now routes to the
    log fold, which is the right shape for generated markup: the same element
    repeated with a number changed.
    """
    print("\nhtml")
    page = (
        "<!DOCTYPE html>\n<html>\n<head><title>Report</title></head>\n<body>\n"
        + "".join(
            '  <div class="row" id="r{i}"><span class="k">metric {i}</span>'
            '<span class="v">{v}</span></div>\n'.format(i=i, v=i * 7 % 1000)
            for i in range(600)
        )
        + "</body>\n</html>\n"
    )
    check(detect(page) == "html", "the page is detected as html", detect(page))
    out = compress_raw(page, "recoverable")
    removed = 100.0 * (len(page) - len(out)) / len(page)
    check(
        removed > 50.0,
        "a repetitive page is folded rather than waved through",
        f"only {removed:.2f}% removed",
    )
    check(
        "ref:k_" in out,
        "and the folded elements keep a handle, so their differences are fetchable",
        out[-200:],
    )
    # A page with nothing repeated has nothing to fold, and saying so is the
    # honest answer rather than a failure.
    unique = "<html>\n" + "".join(
        "<p>{}</p>\n".format("word%d " % i * (i % 7 + 1)) for i in range(200)
    )
    same = compress_raw(unique, "recoverable")
    check(
        len(same) <= len(unique),
        "a page with nothing repeated is never made larger",
        f"{len(unique)} -> {len(same)}",
    )


def a_truncated_frame_does_not_defeat_the_parser() -> None:
    """The frame codex writes when it cuts a tool result short.

    Its truncation notice sits *under* the `Output:` terminator, and a splitter
    that stops at the terminator leaves two lines of prose at the head of the
    body — which is a syntax error to every grammar, so the code compressor
    declines and a 40 KB Python result comes back whole. Measured through this
    surface: 0.00% with the notice in the body, ~87% with it in the frame.
    """
    print("\na truncated harness frame")
    body = "\n\n".join(
        "def transform_{n}(records, options=None):\n".format(n=n)
        + '    """Transform pass {n}."""\n'.format(n=n)
        + "".join(
            "    step_{k} = compute(records, {k}, offset={o})\n".format(k=k, o=n * k)
            for k in range(30)
        )
        + "    return records\n"
        for n in range(20)
    )
    framed = (
        "Chunk ID: e2e001\n"
        "Wall time: 0.0000 seconds\n"
        "Process exited with code 0\n"
        "Original token count: 16692\n"
        "Output:\n"
        "Warning: truncated output (original token count: 16692)\n"
        "Total output lines: 1577\n"
        "\n" + body
    )
    out = compress_raw(framed, "recoverable")
    removed = 100.0 * (len(framed) - len(out)) / len(framed)
    check(
        removed > 50.0,
        "a code payload under a truncated frame is still compressed",
        f"only {removed:.2f}% removed — the notice is being left in the body",
    )
    check(
        out.startswith("Chunk ID: e2e001\n"),
        "the frame itself comes back verbatim",
        out[:40],
    )
    for line in ("Warning: truncated output", "Total output lines: 1577"):
        check(line in out, f"the frame keeps {line!r}", out[:200])


def cross_turn_fold_needs_no_handle() -> None:
    """The one elision in `recoverable` that mints nothing, end to end.

    Every other fold in this mode retains what it dropped, and the suite asserts
    that. The cross-message fold is the exception and it changes what the mode
    promises, so it is checked as its own guarantee rather than left to the
    unit tests: what leaves the machine is the thing this suite exists to look
    at, and this behaviour is only reachable across *messages*, which no
    single-payload check can reach.

    The trade is only sound if the text is genuinely still there. A fold that
    elided the repeat *and* let the first copy be compressed would be lossy in
    the one mode that promises it is not — so the earlier message carrying the
    run verbatim is the assertion that matters here, not the saving.
    """
    print("\ncross-message fold")
    run = "\n".join(
        f"def handler_{i}(request, context):  # repeated verbatim across turns {i}"
        for i in range(40)
    )

    reset_provider()
    body = {
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "here is the module:\n" + run},
            {"role": "user", "content": "here it is again:\n" + run},
        ],
    }
    status, _, _ = post(
        f"{PROXY}/v1/chat/completions", body, {"x-kompressor-mode": "recoverable"}
    )
    check(status == 200, "[cross-turn] the proxy relays it", f"status {status}")

    seen = recorded()
    if not check(seen, "[cross-turn] the upstream was reached"):
        return
    upstream = seen[0]["body"]
    sent = json.dumps(body)

    check(
        len(upstream) < len(sent),
        "[cross-turn] the upstream received a smaller body",
        f"{len(sent)} sent, {len(upstream)} received",
    )
    check(
        "shown in full earlier in this conversation" in upstream,
        "[cross-turn] the fold announces itself and says where the text went",
    )

    # The point of the whole exercise: no handle, and none needed.
    folded = json.loads(upstream)["messages"][-1]["content"]
    check(
        not HANDLE.findall(folded),
        "[cross-turn] the later message keeps no handle",
        "a fold whose text is still in the conversation should mint nothing",
    )
    check(
        run in json.loads(upstream)["messages"][0]["content"],
        "[cross-turn] and the earlier message still carries the run verbatim",
        "the fold elided a repeat whose original did not survive, which is lossy",
    )


def declines_are_honest() -> None:
    """The two ways the proxy is *supposed* to remove nothing.

    A request that compresses nothing is not a failure, and the engine names
    which kind it was — `prefix_frozen`, `no_compressible_content`. Nothing
    checked either end to end, and the first is the one that matters: a client
    that pins its prompt with `cache_control` must get its bytes to the provider
    untouched, because rewriting one of them drops the cache hit rate to zero
    and raises the bill silently. Measured on a real Claude Code session, that
    is every message it sends.
    """
    print("\nhonest declines")
    payload = log_payload()

    # 1. A trailing cache_control marker pins the whole prompt. The compressible
    #    payload is in there and must still arrive byte for byte.
    reset_provider()
    body = {
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 64,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": payload},
                    {
                        "type": "text",
                        "text": "summarise it",
                        "cache_control": {"type": "ephemeral"},
                    },
                ],
            }
        ],
    }
    status, _, _ = post(f"{PROXY}/v1/messages", body)
    seen = recorded()
    check(status == 200, "a pinned prompt is relayed", f"status {status}")
    # Asserted on the serialized body rather than by walking it: the guarantee
    # is about bytes, and a shape-specific walk would pass a request whose
    # structure changed underneath it.
    # Any recorded body, not the last: a turn the mock answers with a tool call
    # makes the proxy send a second round, and the invariant is about what the
    # provider was shown — not about which of those requests was final.
    if seen:
        needle = json.dumps(payload)[1:-1]
        check(
            any(needle in r.get("body", "") for r in seen),
            "a cache_control-pinned prompt reaches the provider byte for byte",
            f"the frozen prefix was rewritten in all {len(seen)} recorded request(s)",
        )
    else:
        check(False, "a cache_control-pinned prompt reaches the provider", "nothing recorded")

    # 2. Prose with no structure: eligible, looked at, and correctly left alone.
    reset_provider()
    prose = (
        "The quick brown fox jumps over the lazy dog near the riverbank at dusk. "
        "Nothing in this sentence repeats, tabulates or nests, and there is no "
        "structure here for any compressor to take hold of. "
    ) * 8
    body = {"model": "gpt-4", "messages": [{"role": "user", "content": prose}]}
    status, _, _ = post(f"{PROXY}/v1/chat/completions", body)
    seen = recorded()
    check(status == 200, "unstructured prose is relayed", f"status {status}")
    if seen:
        needle = json.dumps(prose)[1:-1]
        check(
            any(needle in r.get("body", "") for r in seen),
            "prose the engine cannot compress is forwarded unchanged",
            "the engine altered a payload it had nothing to remove from",
        )
    else:
        check(False, "unstructured prose reaches the provider", "nothing recorded")


def keepwarm_body(tag: str, stop: str, background: str | None = None) -> dict:
    """An Anthropic conversation the mock answers as a cached one (`KEEPWARM:`).

    `background` is `None` for no background task, `"running"` for one Claude
    Code reported starting, and `"reported"` for one whose notification came.
    `tag` keeps each check's conversation its own.
    """
    messages = [
        {"role": "user", "content": f"keep-warm check {tag}: build the project."},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_0", "name": "bash", "input": {"command": "make"}}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_0", "content": "ok"}]},
    ]
    if background:
        messages += [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_bg", "name": "bash",
                 "input": {"command": "make train", "run_in_background": True}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_bg",
                 "content": "Command running in background with ID: bkw0001. "
                            "You will be notified when it completes."}]},
        ]
    if background == "reported":
        messages.append({"role": "user", "content": [{"type": "text", "text":
            "[SYSTEM NOTIFICATION - NOT USER INPUT]\n<task-notification>\n"
            "<task-id>bkw0001</task-id>\n<status>completed</status>"}]})
    return {
        "model": "claude-opus-5-5",
        "max_tokens": 4096,
        "system": [{"type": "text", "text": f"KEEPWARM:{stop}",
                    "cache_control": {"type": "ephemeral"}}],
        "messages": messages,
    }


def pings_seen() -> list[dict]:
    pings = []
    for request in recorded():
        if not request["path"].endswith("/messages"):
            continue
        try:
            body = json.loads(request["body"])
        except ValueError:
            continue
        if body.get("max_tokens") == 1:
            pings.append({"body": body, "headers": request["headers"]})
    return pings


def keep_warm() -> None:
    """The proxy keeps a waiting conversation's provider cache alive.

    Interval 2s, at most 3 pings, at most 2 for a background wait — set in
    `kompressor.toml` so the suite can watch the rule work in seconds. Every
    case is asserted on what the provider received.
    """
    print("\n== keep-warm")
    interval = 2.0

    # 1. A tool_use stop: pinged with the request the provider cached, to the cap.
    reset_provider()
    sent = keepwarm_body("tool-use", "tool_use")
    status, _, _ = post(f"{PROXY}/v1/messages", sent)
    check(status == 200, "[keep-warm] a tool_use turn is relayed", f"status {status}")
    time.sleep(interval * 3 + 3)
    pings = pings_seen()
    check(len(pings) == 3, "[keep-warm] it is pinged every interval, up to the cap",
          f"{len(pings)} pings")
    forwarded = next(
        (json.loads(r["body"]) for r in recorded()
         if r["path"].endswith("/messages") and json.loads(r["body"]).get("max_tokens") != 1),
        None,
    )
    if pings and forwarded:
        expected = dict(forwarded, max_tokens=1, stream=False)
        check(pings[0]["body"] == expected,
              "[keep-warm] a ping is the forwarded request with only max_tokens and stream changed",
              json.dumps(pings[0]["body"])[:300])
        ids = [p["headers"].get("x-request-id", "") for p in pings]
        check(all(re.search(rf"\.keepwarm{n + 1}$", i) for n, i in enumerate(ids)),
              "[keep-warm] each ping carries its own numbered request id", str(ids))

    # 2. A turn that ended, with nothing running: never pinged.
    reset_provider()
    post(f"{PROXY}/v1/messages", keepwarm_body("end-turn", "end_turn"))
    time.sleep(interval * 2 + 2)
    check(not pings_seen(), "[keep-warm] an ended turn with nothing running is not pinged",
          f"{len(pings_seen())} pings")

    # 3. A turn that ended while a background task runs: pinged, to its own cap.
    reset_provider()
    post(f"{PROXY}/v1/messages", keepwarm_body("background", "end_turn", "running"))
    time.sleep(interval * 3 + 3)
    check(len(pings_seen()) == 2,
          "[keep-warm] an ended turn with a background task running is pinged, to the background cap",
          f"{len(pings_seen())} pings")

    # 4. The same once the task is reported: not pinged.
    reset_provider()
    post(f"{PROXY}/v1/messages", keepwarm_body("reported", "end_turn", "reported"))
    time.sleep(interval * 2 + 2)
    check(not pings_seen(), "[keep-warm] a background task already reported keeps nothing warm",
          f"{len(pings_seen())} pings")

    # 5. The client's next request cancels the timer before it is sent.
    reset_provider()
    first = keepwarm_body("cancel", "tool_use")
    post(f"{PROXY}/v1/messages", first)
    time.sleep(interval + 1)
    before = len(pings_seen())
    following = json.loads(json.dumps(first))
    following["system"][0]["text"] = "KEEPWARM:end_turn"
    following["messages"] += [
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_kw", "name": "bash", "input": {"command": "make"}}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_kw", "content": "built"}]},
    ]
    post(f"{PROXY}/v1/messages", following)
    time.sleep(interval * 2 + 2)
    check(before == 1 and len(pings_seen()) == 1,
          "[keep-warm] the client's next request stops the pings",
          f"{before} before, {len(pings_seen())} after")


def efficiency(floors: dict) -> None:
    """Compression per detected type, through the HTTP surface, in every mode.

    `make bench` measures the engine; this measures a deployment. The two
    should agree closely, and a gap between them is worth knowing about because
    it lives in the walkers or the serialization rather than in a compressor.
    """
    print("\ncompression efficiency (chars removed, by detected type)")
    if not os.path.exists(CORPUS):
        notes.append(f"corpus not mounted at {CORPUS}; efficiency was not measured")
        print(f"  \033[33mskipped\033[0m no corpus at {CORPUS}")
        return

    payloads = []
    with gzip.open(CORPUS, "rt") as handle:
        for line in handle:
            record = json.loads(line)
            content = record.get("content")
            if isinstance(content, str) and content:
                payloads.append(content)

    totals: dict[str, dict[str, list[int]]] = defaultdict(
        lambda: {mode: [0, 0] for mode in MODES}
    )
    counts: dict[str, int] = defaultdict(int)
    # The same payloads grouped by size: a regression confined to one band is
    # invisible in the aggregate. Boundaries match `size_bands` in the bench.
    # There is no band above 32 KB because the harness truncates tool output at
    # roughly 40 KB, so production has no payload larger than that — the tail
    # this once had a band for turned out to be base64 image blocks, which the
    # wire walker never offers to the engine at all.
    bands: dict[str, dict[str, list[int]]] = defaultdict(
        lambda: {mode: [0, 0] for mode in MODES}
    )
    band_counts: dict[str, int] = defaultdict(int)
    for content in payloads:
        kind = detect(content)
        counts[kind] += 1
        n = len(content)
        band = "<8k" if n < 8192 else "8k-32k" if n < 32768 else ">=32k"
        band_counts[band] += 1
        for mode in MODES:
            out = compress_raw(content, mode)
            for bucket in (totals[kind][mode], bands[band][mode]):
                bucket[0] += n
                bucket[1] += len(out)

    print(f"  {'type':<16}{'n':>5}  " + "".join(f"{m:>14}" for m in MODES))
    for kind in sorted(totals):
        row = f"  {kind:<16}{counts[kind]:>5}  "
        for mode in MODES:
            chars_in, chars_out = totals[kind][mode]
            pct = 100.0 * (chars_in - chars_out) / chars_in if chars_in else 0.0
            row += f"{pct:>13.2f}%"
        print(row)

    overall = {mode: [0, 0] for mode in MODES}
    for kind in totals:
        for mode in MODES:
            overall[mode][0] += totals[kind][mode][0]
            overall[mode][1] += totals[kind][mode][1]
    row = f"  {'ALL':<16}{len(payloads):>5}  "
    for mode in MODES:
        chars_in, chars_out = overall[mode]
        pct = 100.0 * (chars_in - chars_out) / chars_in if chars_in else 0.0
        row += f"{pct:>13.2f}%"
    print("\033[1m" + row + "\033[0m")

    order = ["<8k", "8k-32k", ">=32k"]
    print(f"\n  {'size':<16}{'n':>5}  " + "".join(f"{m:>14}" for m in MODES))
    for band in order:
        if band not in bands:
            continue
        row = f"  {band:<16}{band_counts[band]:>5}  "
        for mode in MODES:
            chars_in, chars_out = bands[band][mode]
            pct = 100.0 * (chars_in - chars_out) / chars_in if chars_in else 0.0
            row += f"{pct:>13.2f}%"
        print(row)

    print("\n  floors")
    for key, floor in sorted(floors.items()):
        kind, _, mode = key.partition("@")
        if mode not in MODES:
            continue
        if kind == "ALL":
            chars_in, chars_out = overall[mode]
        elif kind.startswith("size:"):
            band = kind[len("size:"):]
            if band not in bands:
                notes.append(f"floor {key} names a size band this corpus produced none of")
                continue
            chars_in, chars_out = bands[band][mode]
        elif kind in totals:
            chars_in, chars_out = totals[kind][mode]
        else:
            notes.append(f"floor {key} names a type this corpus produced none of")
            continue
        pct = 100.0 * (chars_in - chars_out) / chars_in if chars_in else 0.0
        check(pct >= floor, f"{kind} @ {mode} >= {floor:.2f}%", f"measured {pct:.2f}%")


def main() -> int:
    print("kompressor end-to-end suite")
    wait_for(f"{PROVIDER}/_health")
    wait_for(f"{API}/readyz")

    floors_path = os.path.join(os.path.dirname(__file__), "floors.json")
    with open(floors_path) as handle:
        floors = json.load(handle)["floors"]

    surfaces_answer()
    handles = proxy_flow_in_every_mode()
    subscription_routing()
    never_larger_and_deterministic()
    refs_and_retrievals(handles.get("recoverable", []))
    handles_are_never_dangling()
    retrieve_loops()
    a_stream_the_provider_does_not_label_is_still_relayed()
    a_truncated_frame_does_not_defeat_the_parser()
    html_is_compressed_rather_than_waved_through()
    a_generated_config_is_folded_rather_than_barely_touched()
    generated_text_folds_on_its_template_not_only_on_exact_repeats()
    the_control_arm_forwards_unchanged_and_says_so()
    json_behind_a_harness_frame_is_still_json()
    cross_turn_fold_needs_no_handle()
    declines_are_honest()
    keep_warm()
    efficiency(floors)

    for note in notes:
        print(f"\n  \033[33mnote\033[0m {note}")
    if failures:
        print(f"\n\033[31m{len(failures)} check(s) failed\033[0m")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("\n\033[32mall good\033[0m")
    return 0


if __name__ == "__main__":
    sys.exit(main())
