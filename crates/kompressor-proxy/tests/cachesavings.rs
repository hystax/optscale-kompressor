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
/// What the shipped settings saved on [`CAPTURE`] as 1.5.14, recorded after the
/// simulator learned to leave Claude Code's billing header out of the cache
/// key. Small, because the retrieve tool's definition, sent on every request,
/// costs about three quarters of what compression removes from this traffic
/// (measured with `retrieve_tool = "never"` on three 1.5.14 captures). A change
/// that does better moves this up.
const SAVED_AT_COMMIT: f64 = 0.0060;

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

/// Client Messages requests from a body capture, `.gz` or plain, each with
/// its capture time and the conversation it belongs to when the capture says.
fn captured_requests(path: &str) -> Vec<(u64, String, Value)> {
    use std::io::Read;
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut text = String::new();
    if path.ends_with(".gz") {
        flate2::read::GzDecoder::new(file).read_to_string(&mut text)
    } else {
        std::io::BufReader::new(file).read_to_string(&mut text)
    }
    .unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut requests = Vec::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
        let conversation = record["conversation"].as_str().unwrap_or("").to_string();
        requests.push((record["at_ms"].as_u64().unwrap_or(0), conversation, body));
    }
    assert!(
        !requests.is_empty(),
        "no client Messages requests in {path}"
    );
    requests
}

/// Replays a captured run's client requests through both arms.
///
/// Plays the committed [`CAPTURE`] and gates on it, so a change that makes
/// real Claude traffic cost more on the input side fails `make test`. Point
/// `KOMPRESSOR_REPLAY_BODIES` at another run's `kompressor-bodies.jsonl` (the
/// `KOMPRESSOR_BODY_CAPTURE` file) to price that one instead, reported and not
/// gated; `KOMPRESSOR_REPLAY_DETAIL=1` adds a row per conversation.
///
/// The simulated clock follows the capture's timestamps, so real pauses expire
/// real entries; the proxy's own clock does not, so its adaptive one-hour TTL,
/// which switches on a pause it has to see, never switches here. Any
/// `KOMPRESSOR__…` override in the environment applies to the proxied arm,
/// which is how to price an experimental setting against the shipped one:
///
/// ```bash
/// KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__MIN_ELISION_SAVING_CHARS=2000 \
/// make test-cache-replay BODIES=runs/…/kompressor-bodies.jsonl
/// ```
#[tokio::test]
async fn a_captured_run_replayed_through_both_arms() {
    let chosen = std::env::var("KOMPRESSOR_REPLAY_BODIES").ok();
    let path = chosen.clone().unwrap_or_else(|| CAPTURE.to_string());
    let requests = captured_requests(&path);

    let direct = CacheSim::new();
    let proxied = CacheSim::new();
    let harness = proxy_in_front_of(&proxied).await;
    for (at, _, body) in &requests {
        direct.at_ms(*at);
        proxied.at_ms(*at);
        direct.bill(body);
        harness.post("/v1/messages", body).await;
    }
    let offered = harness
        .upstream_bodies()
        .await
        .iter()
        .filter(|body| body.contains("\"name\":\"optscale_kompressor_retrieve\""))
        .count();
    eprintln!(
        "{} client requests from {path}; the retrieve tool was offered on {offered}",
        requests.len()
    );
    let (direct_log, proxied_log) = (direct.log(), proxied.log());
    assert_eq!(
        proxied_log.len(),
        requests.len(),
        "one upstream call per request: the simulator never asks for a retrieval"
    );
    if std::env::var_os("KOMPRESSOR_REPLAY_DETAIL").is_some() {
        let mut each: std::collections::BTreeMap<&str, (Ledger, Ledger)> = Default::default();
        for (i, (_, conversation, _)) in requests.iter().enumerate() {
            let entry = each.entry(conversation).or_default();
            entry.0.add(&direct_log[i].0, &direct_log[i].1);
            entry.1.add(&proxied_log[i].0, &proxied_log[i].1);
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
    }
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
