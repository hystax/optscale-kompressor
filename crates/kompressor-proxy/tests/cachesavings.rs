//! What the proxy saves on a Claude conversation, priced by a provider that
//! caches the way Anthropic's does.
//!
//! Each test plays one conversation twice against [`CacheSim`]: straight from
//! the client, as if no proxy were there, and through the proxy under the
//! shipped `packaging/kompressor.toml`. Both arms are billed by the same
//! simulated cache, so the difference is what the proxy changed: fewer tokens
//! sent, and whether the bytes it sent were still the bytes the provider had
//! cached.
//!
//! What this cannot see is the model. It answers "ok" to everything, so output
//! cost, retrieval rounds and extra agent steps — the part of a real bill a
//! compressor can make *worse* — are not measured here. A saving reported here
//! is the input-side ceiling; Harbor runs say how much of it survives.
//!
//! Run with `--nocapture` to see the ledgers.

mod support;

use serde_json::{Value, json};
use support::Harness;
use support::cachesim::{CacheSim, LOOKBACK, Ledger, Usage};
use wiremock::Mock;
use wiremock::matchers::any;

const MODEL: &str = "claude-opus-5-5";
/// What the shipped settings save on [`CAPTURE`], recorded when the replay put
/// the proxy on the capture's clock. Before that the proxy never saw a pause
/// and the same build measured 1.13%; the rest is its adaptive one-hour TTL,
/// which now switches on the three requests that follow a pause of more than
/// five minutes, as it does in a deployment. Small either way, because the
/// retrieve tool's definition, sent on every request, costs about three
/// quarters of what compression removes from this traffic (measured with
/// `retrieve_tool = "never"` on three 1.5.14 captures). A change that does
/// better moves this up.
const SAVED_AT_COMMIT: f64 = 0.0228;

/// The configuration a package installs, so the test measures what ships.
/// Environment overrides apply on top, as they do in a deployment, which is
/// how the replay below tries an experimental arm.
fn packaged() -> kompressor_core::config::Config {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/kompressor.toml"
    );
    kompressor_core::config::Config::load(Some(std::path::Path::new(path)))
        .expect("the packaged configuration loads")
}

/// A proxy under the packaged configuration, in front of `sim`.
async fn proxy_in_front_of(sim: &CacheSim) -> Harness {
    let harness = Harness::with(|config| {
        let upstreams = std::mem::take(&mut config.proxy.upstreams);
        *config = packaged();
        config.proxy.upstreams = upstreams;
        config.stats.enabled = false;
    })
    .await;
    Mock::given(any())
        .respond_with(sim.clone())
        .mount(&harness.server)
        .await;
    harness
}

/// A conversation shaped like Claude Code's: a tool list and a system prompt
/// that open every request, then a task and a run of `Bash` calls, with the
/// client's `cache_control` on its last system block and on the final block of
/// its final message.
struct Conversation {
    messages: Vec<Value>,
    calls: usize,
}

impl Conversation {
    fn new(task: &str) -> Conversation {
        Conversation {
            messages: vec![json!({"role": "user", "content": [{"type": "text", "text": task}]})],
            calls: 0,
        }
    }

    /// The model ran `command` and got `output` back.
    fn bash(&mut self, command: &str, output: &str) {
        let id = format!("toolu_{:03}", self.calls);
        self.calls += 1;
        self.messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}
        ]}));
        self.messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": output}
        ]}));
    }

    fn body(&self) -> Value {
        let mut messages = self.messages.clone();
        if let Some(last) = messages
            .last_mut()
            .and_then(|message| message["content"].as_array_mut())
            .and_then(|blocks| blocks.last_mut())
        {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        json!({
            "model": MODEL,
            "max_tokens": 32000,
            "tools": tools(),
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": system_prompt(), "cache_control": {"type": "ephemeral"}}
            ],
            "messages": messages
        })
    }
}

fn tools() -> Vec<Value> {
    ["Bash", "Read", "Edit", "Write", "Grep", "Glob"]
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("{name}: {}", "Performs the operation the tool is named for, with the usual caveats about paths, permissions and output limits. ".repeat(12)),
                "input_schema": {"type": "object",
                                 "properties": {"command": {"type": "string"}},
                                 "required": ["command"]}
            })
        })
        .collect()
}

fn system_prompt() -> String {
    "Work carefully, read before you edit, run the tests after a change, and report what you did. "
        .repeat(60)
}

/// A build that compiles a few hundred units and warns about some of them.
fn build_log(units: usize) -> String {
    (0..units)
        .map(|i| {
            if i % 37 == 5 {
                format!(
                    "warning: unused variable `tmp{i}` in src/module_{i}.rs:{}:9",
                    i % 200
                )
            } else {
                format!(
                    "   Compiling crate-{i:03} v0.{}.{} (/build/deps/crate-{i:03})",
                    i % 9,
                    i % 13
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A test run that mostly passes.
fn test_log(cases: usize, failing: usize) -> String {
    let mut lines: Vec<String> = (0..cases)
        .map(|i| {
            let verdict = if i == failing { "FAILED" } else { "ok" };
            format!(
                "test suite::case_{i:04}_handles_input_variant_{} ... {verdict}",
                i % 7
            )
        })
        .collect();
    lines.push(format!(
        "\nfailures:\n    suite::case_{failing:04}_handles_input_variant_{}\n\ntest result: FAILED. {} passed; 1 failed; 0 ignored",
        failing % 7,
        cases - 1
    ));
    lines.join("\n")
}

/// A source file the model printed with `cat`.
fn source(functions: usize) -> String {
    (0..functions)
        .map(|i| {
            format!(
                "pub fn handler_{i}(input: &Request) -> Result<Response, Error> {{\n    let parsed = parse_{i}(input)?;\n    if parsed.len() > {} {{\n        return Err(Error::TooLong(parsed.len()));\n    }}\n    Ok(Response::new(parsed))\n}}\n",
                i * 3 + 10
            )
        })
        .collect()
}

/// The session every test plays: a debugging run heavy on Bash output, with a
/// build log the model asks for twice, as agents do after a fix.
fn session() -> Vec<Value> {
    let mut conversation =
        Conversation::new("The build is failing and three tests are red. Find out why and fix it.");
    let mut bodies = vec![conversation.body()];
    let steps: Vec<(&str, String)> = vec![
        (
            "git status --short",
            " M src/lib.rs\n M src/parse.rs\n?? notes.txt".to_string(),
        ),
        ("cargo build 2>&1", build_log(400)),
        ("cat src/handlers.rs", source(40)),
        ("cargo test 2>&1", test_log(500, 217)),
        (
            "ls -la src",
            (0..60)
                .map(|i| {
                    format!(
                        "-rw-r--r-- 1 dev dev {:>6} Nov  2 10:{:02} file_{i}.rs",
                        1000 + i * 37,
                        i % 60
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("sed -n 1,40p src/parse.rs", source(6)),
        ("cargo build 2>&1", build_log(400)),
        ("cargo test 2>&1", test_log(500, 501)),
        (
            "git diff --stat",
            " src/parse.rs | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)".to_string(),
        ),
    ];
    for (command, output) in steps {
        conversation.bash(command, &output);
        bodies.push(conversation.body());
    }
    bodies
}

/// The same session, resumed: the first request this proxy sees already
/// holds a build log, so that log sits under the client's cache marker from the
/// start and stays uncompressed for as long as the provider is serving it.
fn resumed_session() -> Vec<Value> {
    let mut conversation =
        Conversation::new("The build is failing and three tests are red. Find out why and fix it.");
    conversation.bash("cargo build 2>&1", &build_log(400));
    let mut bodies = vec![conversation.body()];
    for (command, output) in [
        (
            "git status --short",
            " M src/lib.rs\n M src/parse.rs".to_string(),
        ),
        ("cargo test 2>&1", test_log(500, 217)),
        ("cat src/handlers.rs", source(40)),
        ("cargo test 2>&1", test_log(500, 501)),
        ("git diff --stat", " src/parse.rs | 4 ++--".to_string()),
    ] {
        conversation.bash(command, &output);
        bodies.push(conversation.body());
    }
    bodies
}

/// Bill `bodies` as the client sent them, and as the proxy forwarded them,
/// with `between` run before each request on both simulated clocks.
async fn both_arms(
    bodies: &[Value],
    mut between: impl FnMut(usize, &CacheSim),
) -> (CacheSim, CacheSim) {
    let direct = CacheSim::new();
    let proxied = CacheSim::new();
    let harness = proxy_in_front_of(&proxied).await;
    for (turn, body) in bodies.iter().enumerate() {
        between(turn, &direct);
        between(turn, &proxied);
        direct.bill(body);
        let reply = harness.post("/v1/messages", body).await;
        assert_eq!(reply.status, 200, "turn {turn}: {}", reply.text());
    }
    assert_eq!(
        proxied.log().len(),
        bodies.len(),
        "one upstream call per turn: no retrieval round can happen here"
    );
    (direct, proxied)
}

fn report(direct: &Ledger, proxied: &Ledger) -> f64 {
    let saved = 1.0 - proxied.dollars / direct.dollars;
    eprintln!("{}", direct.row("direct"));
    eprintln!("{}", proxied.row("proxied"));
    eprintln!("input-side saving: {:.2}%", saved * 100.0);
    saved
}

/// Turns whose cache read fell short of what the turn before had cached.
fn lost_reuse(log: &[(String, Usage)]) -> Vec<(usize, u64)> {
    log.windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[1].1.read < pair[0].1.frontier)
        .map(|(i, pair)| (i + 1, pair[0].1.frontier - pair[1].1.read))
        .collect()
}

#[tokio::test]
async fn the_shipped_settings_cost_less_than_sending_the_session_direct() {
    let bodies = session();
    let (direct, proxied) = both_arms(&bodies, |_, _| {}).await;
    let saved = report(&direct.ledger(), &proxied.ledger());

    // Measured when this test was written: 1.5.14 saves 69.8% of this
    // session's input-side cost. That is a property of the fixture, whose
    // generated output repeats far more than real Bash output does, and not a
    // forecast: replayed over the 89-task 1.5.12 and 1.5.14 captures the same
    // settings come out between -0.2% and +0.4%, because the retrieve tool's
    // definition costs most of what compression removes. The floor is here to
    // fail a change that stops
    // compressing this output, or starts breaking the cache to do it.
    assert!(
        saved > 0.40,
        "the proxy should save at least 40% of input cost here; saved {:.2}%",
        saved * 100.0
    );
}

#[tokio::test]
async fn no_turn_loses_what_the_turn_before_it_cached() {
    // The invariant every compression decision answers to: appending a turn
    // must not change the bytes the previous turn sent. A turn that does
    // reads less than the turn before cached, and pays to write it again.
    //
    // Both sessions, because they protect different things: a fresh one has
    // everything compressed as it arrives, a resumed one carries history the
    // proxy must leave as the provider cached it.
    for (name, bodies) in [("fresh", session()), ("resumed", resumed_session())] {
        let (direct, proxied) = both_arms(&bodies, |_, _| {}).await;
        assert_eq!(
            lost_reuse(&direct.log()),
            vec![],
            "{name}: the client's own history is stable, so the simulation reads it back"
        );
        assert_eq!(
            lost_reuse(&proxied.log()),
            vec![],
            "{name}: (turn, tokens) the proxy's rewriting of history cost"
        );
    }
}

#[tokio::test]
async fn a_lapse_is_paid_for_once() {
    // An agent waits seven minutes on a build and the five-minute entry
    // lapses. That turn rewrites the cache in both arms; what matters is the
    // turn after it, which must read what the lapse turn wrote back rather
    // than rewrite the history a second time.
    //
    // Played on a resumed session because that is where the lapse has
    // something to get wrong: the log the first request arrived with is still
    // uncompressed, and a proxy that read the lapse as licence to compress it
    // would change bytes the lapse turn had just written back.
    let bodies = resumed_session();
    let lapse = 3;
    let (direct, proxied) = both_arms(&bodies, |turn, sim| {
        if turn == lapse {
            sim.advance_ms(7 * 60 * 1000);
        }
    })
    .await;
    for (name, sim) in [("direct", &direct), ("proxied", &proxied)] {
        let log = sim.log();
        assert_eq!(
            log[lapse].1.read, 0,
            "{name}: the lapse turn finds nothing cached"
        );
        assert_eq!(
            lost_reuse(&log[lapse..]),
            vec![],
            "{name}: every turn after the lapse reads what the one before wrote"
        );
    }
    report(&direct.ledger(), &proxied.ledger());
}

// The simulator's own rules, checked once here rather than in every test
// binary that includes the support module.

fn simple(messages: Value) -> Value {
    json!({"model": MODEL, "max_tokens": 10, "messages": messages})
}

fn big(tag: &str) -> String {
    format!("{tag} {}", "x".repeat(8_000))
}

fn pinned(text: String) -> Value {
    simple(json!([
        {"role": "user", "content": [{"type": "text", "text": text,
                                      "cache_control": {"type": "ephemeral"}}]}
    ]))
}

#[test]
fn the_simulator_reads_a_repeated_prefix_and_writes_the_new_suffix() {
    let sim = CacheSim::new();
    let one = sim.bill(&pinned(big("a")));
    assert_eq!(one.read, 0);
    assert!(one.write_5m > 0);
    let two = sim.bill(&simple(json!([
        {"role": "user", "content": [{"type": "text", "text": big("a")}]},
        {"role": "assistant", "content": "ok"},
        {"role": "user", "content": [{"type": "text", "text": big("b"),
                                      "cache_control": {"type": "ephemeral"}}]}
    ])));
    assert_eq!(
        two.read, one.frontier,
        "the first turn's prefix is read back"
    );
    assert!(two.write_5m > 0, "and the new turn written");
}

#[test]
fn the_simulator_loses_everything_after_a_changed_byte() {
    let sim = CacheSim::new();
    sim.bill(&pinned(big("a")));
    assert_eq!(sim.bill(&pinned(big("A"))).read, 0);
}

#[test]
fn the_simulator_expires_an_entry_nothing_reads() {
    let sim = CacheSim::new();
    let request = pinned(big("a"));
    sim.bill(&request);
    sim.advance_ms(4 * 60 * 1000);
    assert!(sim.bill(&request).read > 0, "four minutes: still there");
    sim.advance_ms(4 * 60 * 1000);
    assert!(sim.bill(&request).read > 0, "renewed by the read before");
    sim.advance_ms(6 * 60 * 1000);
    assert_eq!(sim.bill(&request).read, 0, "six idle minutes: gone");
}

#[test]
fn the_simulator_keeps_a_one_hour_entry_through_a_pause() {
    let sim = CacheSim::new();
    let request = simple(json!([
        {"role": "user", "content": [{"type": "text", "text": big("a"),
                                      "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}
    ]));
    assert!(sim.bill(&request).write_1h > 0);
    sim.advance_ms(30 * 60 * 1000);
    assert!(sim.bill(&request).read > 0);
}

#[test]
fn the_simulator_looks_back_twenty_blocks_and_no_further() {
    let sim = CacheSim::new();
    sim.bill(&pinned(big("a")));
    let mut messages =
        vec![json!({"role": "user", "content": [{"type": "text", "text": big("a")}]})];
    for i in 0..LOOKBACK {
        messages.push(json!({"role": "user", "content": format!("turn {i}")}));
    }
    messages.last_mut().unwrap()["content"] =
        json!([{"type": "text", "text": "last", "cache_control": {"type": "ephemeral"}}]);
    assert_eq!(
        sim.bill(&simple(Value::Array(messages.clone()))).read,
        0,
        "the entry is outside the twenty blocks the breakpoint looks back over"
    );
    // One block nearer and it is found.
    messages.remove(1);
    assert!(sim.bill(&simple(Value::Array(messages))).read > 0);
}

#[test]
fn the_simulator_shares_a_prefix_across_billing_headers() {
    // Two sessions whose only difference is Claude Code's billing header: the
    // second reads what the first wrote, as the provider does.
    let sim = CacheSim::new();
    let session = |header: &str| {
        json!({"model": MODEL, "max_tokens": 10,
               "system": [{"type": "text", "text": header},
                          {"type": "text", "text": big("system"),
                           "cache_control": {"type": "ephemeral"}}],
               "messages": [{"role": "user", "content": "go"}]})
    };
    let first = sim.bill(&session(
        "x-anthropic-billing-header: cc_version=2.1.316.777;",
    ));
    assert!(first.write_5m > 0);
    let second = sim.bill(&session(
        "x-anthropic-billing-header: cc_version=2.1.316.4e7;",
    ));
    assert_eq!(second.read, first.frontier);
}

#[test]
fn the_simulator_does_not_cache_a_short_prefix() {
    let sim = CacheSim::new();
    // Two tokens, against the simulator's 1,024-token minimum.
    let request = pinned("short".to_string());
    assert_eq!(sim.bill(&request).write_5m, 0);
    assert_eq!(sim.bill(&request).read, 0);
}

/// The committed capture: a few whole conversations from the 89-task 1.5.12
/// Opus run (2026-11-01 13-41-26), client side only. Claude Code's system
/// prompt and tool descriptions are replaced with filler of the same length,
/// identical on every request, and `metadata` is dropped: the proxy compresses
/// none of them, so the cache sees exactly what it saw. Task text and tool
/// output are as captured. Built by `scripts/bench/extract-cache-capture.py`.
const CAPTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/claude-capture.jsonl.gz"
);

/// One line of a body capture that a replay acts on.
enum Captured {
    /// A client Messages request.
    Request {
        at_ms: u64,
        request_id: String,
        /// The conversation it belongs to, when the capture says.
        conversation: String,
        body: Value,
    },
    /// A keep-warm ping the original run sent after the request named.
    Ping {
        at_ms: u64,
        /// The request it followed.
        after: String,
    },
}

/// What a body capture, `.gz` or plain, holds for a replay, in capture order:
/// its client Messages requests, and the keep-warm pings sent between them
/// where the capture recorded those (see `bodycapture`).
fn captured(path: &str) -> Vec<Captured> {
    use std::io::Read;
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut text = String::new();
    if path.ends_with(".gz") {
        flate2::read::GzDecoder::new(file).read_to_string(&mut text)
    } else {
        std::io::BufReader::new(file).read_to_string(&mut text)
    }
    .unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut events = Vec::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let at_ms = record["at_ms"].as_u64().unwrap_or(0);
        let request_id = record["request_id"].as_str().unwrap_or("").to_string();
        if record["dir"] == "keep-warm" {
            // A ping that read nothing bridged nothing, and one that was
            // refused was never billed.
            if record["read"].as_u64().unwrap_or(0) > 0
                && let Some((after, _)) = request_id.rsplit_once(".keepwarm")
            {
                events.push(Captured::Ping {
                    at_ms,
                    after: after.to_string(),
                });
            }
            continue;
        }
        if record["dir"] != "client->proxy" {
            continue;
        }
        let Ok(mut body) = serde_json::from_str::<Value>(record["body"].as_str().unwrap_or(""))
        else {
            continue;
        };
        // A Messages call: count_tokens and other routes carry no max_tokens.
        if !body["messages"].is_array() || body["max_tokens"].is_null() {
            continue;
        }
        // The simulator answers in one piece; streaming changes nothing cached.
        body["stream"] = json!(false);
        events.push(Captured::Request {
            at_ms,
            request_id,
            conversation: record["conversation"].as_str().unwrap_or("").to_string(),
            body,
        });
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Captured::Request { .. })),
        "no client Messages requests in {path}"
    );
    events
}

/// One replayed request, in both arms.
struct Replayed {
    at_ms: u64,
    conversation: String,
    model: String,
    direct: Usage,
    proxied: Usage,
    /// Which upstream body the proxied arm sent for it.
    sent: usize,
    /// What the proxy observed of it.
    seen: Option<kompressor_stats::context::PrefixObservation>,
}

/// Where two upstream bodies of one conversation first differ, in the order
/// the provider renders them, breakpoints left out: `tools`, `system`, or the
/// index of the first message of `before` that `now` does not carry as it was.
fn first_change(before: &Value, now: &Value) -> Option<String> {
    fn unmarked(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| *key != "cache_control")
                    .map(|(key, item)| (key.clone(), unmarked(item)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(unmarked).collect()),
            other => other.clone(),
        }
    }
    for key in ["tools", "system"] {
        if unmarked(&before[key]) != unmarked(&now[key]) {
            return Some(key.to_string());
        }
    }
    let empty = Vec::new();
    let (was, is) = (
        before["messages"].as_array().unwrap_or(&empty),
        now["messages"].as_array().unwrap_or(&empty),
    );
    (0..was.len())
        .find(|&i| is.get(i).map(unmarked) != Some(unmarked(&was[i])))
        .map(|i| format!("message {i}"))
}

/// A row for every proxied turn that read back less than the turn before it
/// in its conversation had cached: what was lost and why, beside what the
/// same turn did sent direct.
fn report_losses(turns: &[Replayed], upstream: &[Value]) {
    let mut last: std::collections::HashMap<&str, usize> = Default::default();
    let mut losses = 0;
    for (index, turn) in turns.iter().enumerate() {
        let Some(before) = last.insert(turn.conversation.as_str(), index) else {
            continue;
        };
        let before = &turns[before];
        let lost = before.proxied.frontier.saturating_sub(turn.proxied.read);
        if lost == 0 {
            continue;
        }
        losses += 1;
        let prices = support::cachesim::Prices::for_model(&turn.model);
        let written = |usage: &Usage| (usage.write_5m + usage.write_1h) as i64;
        let changed = first_change(&upstream[before.sent], &upstream[turn.sent]);
        let reads = turn.seen.as_ref().map_or((0, 0, 0), |seen| {
            (
                seen.stale_read_bytes,
                seen.superseded_read_bytes,
                seen.quiet_read_bytes,
            )
        });
        eprintln!(
            "loss  request {index:>4}  {:<12}  idle {:>5} s  lost {:>7} tok (direct lost {:>7})  \
             first change: {:<12}  recompacted: {:<9}  marginal write {:>+8} tok  \
             reads stale/superseded/matured {}/{}/{} B  priced delta ${:+.4}",
            turn.conversation,
            turn.at_ms.saturating_sub(before.at_ms) / 1000,
            lost,
            before.direct.frontier.saturating_sub(turn.direct.read),
            changed.as_deref().unwrap_or("none"),
            turn.seen
                .as_ref()
                .and_then(|seen| seen.recompacted.as_deref())
                .unwrap_or("no"),
            written(&turn.proxied) - written(&turn.direct),
            reads.0,
            reads.1,
            reads.2,
            prices.input_cost(&turn.proxied) - prices.input_cost(&turn.direct),
        );
    }
    eprintln!("{losses} proxied turns read back less than the turn before had cached");
}

/// What replaying a capture through both arms came to.
struct Replay {
    turns: Vec<Replayed>,
    /// The proxied arm's upstream bodies, one per turn.
    upstream: Vec<Value>,
    /// Recorded keep-warm pings replayed, and what the provider billed them.
    pings: u64,
    ping_ledger: Ledger,
    direct: CacheSim,
    proxied: CacheSim,
}

/// Play `events` through both arms on the capture's clock. See
/// [`a_captured_run_replayed_through_both_arms`] for what that means.
async fn replay(events: &[Captured]) -> Replay {
    let direct = CacheSim::new();
    let proxied = CacheSim::new();
    let harness = proxy_in_front_of(&proxied).await;
    let tracker = harness.state.prefixes();
    let mut clock_ms: Option<u64> = None;
    let mut turns: Vec<Replayed> = Vec::new();
    // Each request's place in `turns`, and the conversation the proxy filed
    // it under, by its id in the capture.
    let mut by_id: std::collections::HashMap<String, (usize, Option<u64>)> = Default::default();
    let (mut pings, mut ping_ledger) = (0u64, Ledger::default());

    for event in events {
        let at_ms = match event {
            Captured::Request { at_ms, .. } | Captured::Ping { at_ms, .. } => *at_ms,
        };
        // One clock: both caches and the proxy's tracker move together.
        direct.at_ms(at_ms);
        proxied.at_ms(at_ms);
        if let Some(before) = clock_ms {
            tracker.advance_clock_for_test(std::time::Duration::from_millis(
                at_ms.saturating_sub(before),
            ));
        }
        clock_ms = Some(clock_ms.unwrap_or(at_ms).max(at_ms));

        match event {
            Captured::Request {
                request_id,
                conversation,
                body,
                ..
            } => {
                let billed_direct = direct.bill(body);
                let sent = proxied.log().len();
                harness.post("/v1/messages", body).await;
                let log = proxied.log();
                assert_eq!(
                    log.len(),
                    sent + 1,
                    "one upstream call per request: the simulator never asks for a retrieval"
                );
                let row = support::proxy_rows(&harness.events).pop();
                let filed = row.as_ref().and_then(|row| row.conversation.clone());
                by_id.insert(
                    request_id.clone(),
                    (
                        turns.len(),
                        filed
                            .as_deref()
                            .and_then(|hex| u64::from_str_radix(hex, 16).ok()),
                    ),
                );
                turns.push(Replayed {
                    at_ms,
                    // The capture's own grouping where it has one; else the
                    // proxy's, which is what the original run was filed by.
                    conversation: if conversation.is_empty() {
                        filed.unwrap_or_default()
                    } else {
                        conversation.clone()
                    },
                    model: log[sent].0.clone(),
                    direct: billed_direct,
                    proxied: log[sent].1,
                    sent: turns.len(),
                    seen: row.and_then(|row| row.prefix),
                });
            }
            Captured::Ping { after, .. } => {
                let Some(&(turn, filed)) = by_id.get(after) else {
                    continue;
                };
                // The body this arm's provider cached for that request, as
                // `keepwarm::Ping` sends it.
                let bodies = harness.upstream_json().await;
                let mut ping = bodies[turns[turn].sent].clone();
                ping["max_tokens"] = json!(1);
                let usage = proxied.bill(&ping);
                pings += 1;
                ping_ledger.add(&turns[turn].model, &usage);
                if let (true, Some(conversation)) = (usage.read > 0, filed) {
                    tracker.kept_warm(conversation);
                }
            }
        }
    }

    let upstream = harness.upstream_json().await;
    assert_eq!(upstream.len(), turns.len());
    Replay {
        turns,
        upstream,
        pings,
        ping_ledger,
        direct,
        proxied,
    }
}

/// Replays a captured run's client requests through both arms.
///
/// Plays the committed [`CAPTURE`] and gates on it, so a change that makes
/// real Claude traffic cost more on the input side fails `make test`. Point
/// `KOMPRESSOR_REPLAY_BODIES` at another run's `kompressor-bodies.jsonl` (the
/// `KOMPRESSOR_BODY_CAPTURE` file) to price that one instead, reported and not
/// gated; `KOMPRESSOR_REPLAY_DETAIL=1` adds a row per conversation and a row
/// per lost prefix.
///
/// # One clock
///
/// Three things expire on idle time: the simulated provider's entries, the
/// proxy's own view of a conversation (its adaptive one-hour TTL, a lapse
/// predicted under `cold_recompact = "predicted"`), and the keep-warm pings
/// that bridge a pause. All three follow the capture's timestamps here. Each
/// arm has its own simulated cache on that clock, and the proxy's tracker is
/// moved forward with it before every request. A replay that left the proxy
/// on the machine's clock would play hours in seconds and never show it a
/// pause, so neither setting could fire and neither could be priced.
///
/// The model's answers are not replayed: each request is answered when it is
/// sent, with what the arm's own cache held then. So a response's completion
/// is its request's start, and usage is never carried from the original run
/// into an arm that sent different bytes.
///
/// # Keep-warm pings
///
/// The proxy's own keep-warm timers sleep on the machine's clock and do not
/// fire in a replay. A ping the original run sent is in the capture, where
/// that was recorded (`dir = "keep-warm"`), and is replayed at its time: the
/// proxied arm's cache is billed for the body that arm sent for the request
/// the ping followed, with `max_tokens = 1`, and if it read anything the
/// tracker is told the conversation was kept warm, as `keepwarm` tells it. A
/// capture with no such lines — the committed one, and any made before they
/// were recorded — replays with no pings, and says so. The direct arm has no
/// proxy and so no pings.
///
/// Any `KOMPRESSOR__…` override in the environment applies to the proxied
/// arm, which is how to price an experimental setting against the shipped
/// one:
///
/// ```bash
/// KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__MIN_ELISION_SAVING_CHARS=2000 \
/// make test-cache-replay BODIES=runs/…/kompressor-bodies.jsonl
/// ```
#[tokio::test]
async fn a_captured_run_replayed_through_both_arms() {
    let chosen = std::env::var("KOMPRESSOR_REPLAY_BODIES").ok();
    let path = chosen.clone().unwrap_or_else(|| CAPTURE.to_string());
    let Replay {
        turns,
        upstream,
        pings,
        ping_ledger,
        direct,
        proxied,
    } = replay(&captured(&path)).await;
    let offered = upstream
        .iter()
        .filter(|body| {
            body.to_string()
                .contains("\"name\":\"optscale_kompressor_retrieve\"")
        })
        .count();
    let one_hour = upstream
        .iter()
        .filter(|body| body.to_string().contains("\"ttl\":\"1h\""))
        .count();
    eprintln!(
        "{} client requests from {path}; the retrieve tool was offered on {offered}; \
         {one_hour} asked for one-hour entries",
        turns.len()
    );
    if pings == 0 {
        eprintln!("the capture records no keep-warm pings: every pause is replayed unbridged");
    } else {
        eprintln!("{}", ping_ledger.row("pings"));
    }
    let recompacted = turns
        .iter()
        .filter(|turn| {
            turn.seen
                .as_ref()
                .is_some_and(|seen| seen.recompacted.is_some())
        })
        .count();
    if recompacted > 0 {
        eprintln!("{recompacted} turns were recompacted on a lapsed cache");
    }
    if std::env::var_os("KOMPRESSOR_REPLAY_DETAIL").is_some() {
        let mut each: std::collections::BTreeMap<&str, (Ledger, Ledger)> = Default::default();
        for turn in &turns {
            let entry = each.entry(turn.conversation.as_str()).or_default();
            entry.0.add(&turn.model, &turn.direct);
            entry.1.add(&turn.model, &turn.proxied);
        }
        for (conversation, (d, p)) in &each {
            eprintln!(
                "{conversation:<12} {:>3} req  direct ${:.4}  proxied ${:.4}  saving {:>6.2}%",
                d.requests,
                d.dollars,
                p.dollars,
                (1.0 - p.dollars / d.dollars) * 100.0
            );
        }
        report_losses(&turns, &upstream);
    }
    // The proxied ledger is everything its provider billed, pings included.
    let saved = report(&direct.ledger(), &proxied.ledger());
    if chosen.is_some() {
        return;
    }
    // Deterministic — same bytes, same estimator — so the quarter point of
    // tolerance is room for a deliberate trade, not for noise. A change that
    // improves on the recorded figure should record the new one.
    assert!(
        saved >= SAVED_AT_COMMIT - 0.0025,
        "the shipped settings now cost more on the committed Claude capture: \
         {:.2}% saved, recorded {:.2}%",
        saved * 100.0,
        SAVED_AT_COMMIT * 100.0
    );
}

/// One capture line, as `bodycapture` writes it.
fn line(at_ms: u64, request_id: &str, dir: &str, body: &Value) -> String {
    json!({"at_ms": at_ms, "request_id": request_id, "dir": dir, "body": body.to_string()})
        .to_string()
}

fn ping_line(at_ms: u64, after: &str, read: u64) -> String {
    json!({"at_ms": at_ms, "request_id": format!("{after}.keepwarm1"), "dir": "keep-warm",
           "status": 200, "read": read})
    .to_string()
}

fn capture_file(lines: &[String]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bodies.jsonl");
    std::fs::write(&path, lines.join("\n")).unwrap();
    let path = path.to_str().unwrap().to_string();
    (dir, path)
}

/// A replay acts on a capture's client Messages requests and on the pings
/// that read something, in the order they were written, and on nothing else.
#[test]
fn a_capture_is_read_as_its_requests_and_the_pings_that_kept_something_warm() {
    let request = simple(json!([{"role": "user", "content": "go"}]));
    let (_dir, path) = capture_file(&[
        line(1_000, "r1", "client->proxy", &request),
        line(1_001, "r1", "proxy->upstream", &request),
        // count_tokens carries no max_tokens; not a Messages call.
        line(1_500, "r2", "client->proxy", &json!({"messages": []})),
        "not json at all".to_string(),
        ping_line(200_000, "r1", 4_000),
        // Read nothing: it bridged nothing.
        ping_line(400_000, "r1", 0),
        // A ping whose id names no request is not a ping this can place.
        json!({"at_ms": 5, "request_id": "r9", "dir": "keep-warm", "read": 9}).to_string(),
        line(500_000, "r3", "client->proxy", &request),
    ]);
    let seen: Vec<String> = captured(&path)
        .iter()
        .map(|event| match event {
            Captured::Request {
                at_ms,
                request_id,
                body,
                ..
            } => {
                assert_eq!(body["stream"], false, "answered in one piece");
                format!("request {request_id} at {at_ms}")
            }
            Captured::Ping { at_ms, after } => format!("ping after {after} at {at_ms}"),
        })
        .collect();
    assert_eq!(
        seen,
        [
            "request r1 at 1000",
            "ping after r1 at 200000",
            "request r3 at 500000"
        ]
    );
}

#[test]
fn the_first_change_between_two_bodies_is_found_in_render_order() {
    let body = |tools: Value, system: &str, messages: Value| json!({"tools": tools, "system": system, "messages": messages});
    let messages = json!([
        {"role": "user", "content": [{"type": "text", "text": "a"}]},
        {"role": "assistant", "content": "b"}
    ]);
    let before = body(json!([{"name": "t"}]), "s", messages.clone());
    // An appended turn and a moved breakpoint are not changes.
    let mut appended = before.clone();
    appended["messages"][0]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    appended["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "user", "content": "c"}));
    assert_eq!(first_change(&before, &appended), None);

    let mut rewritten = appended.clone();
    rewritten["messages"][1]["content"] = json!("B");
    assert_eq!(
        first_change(&before, &rewritten).as_deref(),
        Some("message 1")
    );
    let mut shorter = before.clone();
    shorter["messages"].as_array_mut().unwrap().pop();
    assert_eq!(
        first_change(&before, &shorter).as_deref(),
        Some("message 1")
    );
    // Earlier in the prompt wins.
    rewritten["system"] = json!("S");
    assert_eq!(first_change(&before, &rewritten).as_deref(), Some("system"));
    rewritten["tools"] = json!([]);
    assert_eq!(first_change(&before, &rewritten).as_deref(), Some("tools"));
}

/// The session, ten seconds a turn, with an eight-minute pause before its
/// last request — and, when asked, the ping the original run sent four
/// minutes into it.
fn paused_session(with_ping: bool) -> Vec<String> {
    let bodies = session();
    let last = bodies.len() - 1;
    let mut lines = Vec::new();
    for (turn, body) in bodies.iter().enumerate() {
        let at_ms = 1_000_000 + turn as u64 * 10_000 + if turn == last { 480_000 } else { 0 };
        if turn == last && with_ping {
            let before = 1_000_000 + (last as u64 - 1) * 10_000;
            lines.push(ping_line(
                before + 240_000,
                &format!("r{}", last - 1),
                4_000,
            ));
        }
        lines.push(line(at_ms, &format!("r{turn}"), "client->proxy", body));
    }
    lines
}

/// The replay runs on the capture's clock. A pause longer than the cache
/// lifetime expires the simulated entry in both arms, and the proxy sees the
/// same pause: its adaptive TTL asks for an hour on the request that finds it.
#[tokio::test]
async fn a_replayed_pause_is_seen_by_the_cache_and_by_the_proxy() {
    let (_dir, path) = capture_file(&paused_session(false));
    let run = replay(&captured(&path)).await;
    let last = run.turns.last().unwrap();
    let before = &run.turns[run.turns.len() - 2];
    assert_eq!(run.pings, 0);
    assert_eq!(last.direct.read, 0, "eight idle minutes: the entry is gone");
    assert_eq!(last.proxied.read, 0);
    assert!(before.proxied.read > 0 && before.proxied.write_1h == 0);
    assert!(
        last.proxied.write_1h > 0 && last.proxied.write_5m == 0,
        "the proxy saw the pause and asked for an hour: {:?}",
        last.proxied
    );
    let seen = last.seen.as_ref().expect("observed");
    assert!(seen.idle_ms.is_some_and(|idle| idle >= 480_000));
    assert_eq!(seen.continues, Some(true));
    assert_eq!(last.sent, run.turns.len() - 1);
    assert_eq!(
        run.proxied.log().len(),
        run.turns.len(),
        "no ping was billed"
    );
}

/// A ping the original run sent is replayed where it fell: the proxied arm's
/// cache is read and renewed by the body that arm sent, the proxy is told the
/// conversation was kept warm, and the turn after the pause reads its prefix
/// back at the ordinary lifetime. The direct arm, which has no proxy, still
/// loses it.
#[tokio::test]
async fn a_recorded_ping_bridges_the_pause_in_the_proxied_arm_only() {
    let (_dir, path) = capture_file(&paused_session(true));
    let run = replay(&captured(&path)).await;
    let last = run.turns.last().unwrap();
    let before = &run.turns[run.turns.len() - 2];
    assert_eq!(run.pings, 1);
    assert_eq!(last.direct.read, 0, "nothing kept the direct arm warm");
    assert_eq!(
        last.proxied.read, before.proxied.frontier,
        "everything the turn before the pause cached was read back"
    );
    assert_eq!(last.proxied.write_1h, 0, "a bridged pause is not a pause");
    // The ping is billed to the proxied arm, as a read of that turn's prefix.
    assert_eq!(run.ping_ledger.requests, 1);
    assert_eq!(run.ping_ledger.usage.read, before.proxied.frontier);
    assert_eq!(run.proxied.log().len(), run.turns.len() + 1);
    assert!(run.proxied.ledger().dollars > 0.0);
    assert_eq!(run.direct.log().len(), run.turns.len());
    assert_eq!(run.upstream.len(), run.turns.len(), "a ping is not a turn");
    let seen = last.seen.as_ref().expect("observed");
    assert!(
        seen.idle_ms
            .is_some_and(|idle| (240_000..300_000).contains(&idle)),
        "idle since the ping, not since the turn: {:?}",
        seen.idle_ms
    );
}
