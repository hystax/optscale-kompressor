#!/usr/bin/env bash
# Reproduce the Claude Code + kompressor + Anthropic-subscription compression test.
#
#   scripts/kompressor-sweep/claude.sh all        # fixtures, sweep, report
#   scripts/kompressor-sweep/claude.sh fixtures   # build the payload tree only (free)
#   scripts/kompressor-sweep/claude.sh sweep      # the paid part: one claude session
#   scripts/kompressor-sweep/claude.sh report     # saved stage totals, verdicts + HTML (free)
#   scripts/kompressor-sweep/claude.sh retrieval  # paid: can the model fetch back a planted token?
#   scripts/kompressor-sweep/claude.sh bughunt    # paid: code compressor + retrieval, eight languages
#
# `sweep`, `retrieval` and `bughunt` spend Claude-plan quota. Everything else
# is local.
#
# The Claude half of the sweep. `codex.sh` beside it is the ChatGPT one:
# it drives `codex exec`, rewrites `~/.codex/config.toml`, and routes
# subscription traffic to `chatgpt.com/backend-api/codex`. None of that applies
# here. What does carry over is the fixture tree, so this script delegates to
# that one for `fixtures` rather than keeping a second copy of the payloads.
#
#   KOMPRESSOR=/path   the binary to measure (default: the repo's release build)
#   CLAUDE=/path       the Claude Code CLI             (default: $(command -v claude))
#   PORT=8819          the dedicated proxy's port
#   WORK=/path         where fixtures, databases and logs go
#                      reports/<stage>.db keeps each stage's statistics;
#                      kompressor-claude-report.html is the combined report
#   CLAUDE_SWEEP_MODEL=name   passed to `claude --model`; unset uses your default
#   CLAUDE_HOME=/path  a Claude Code config directory of the sweep's own, for a
#                      machine whose ~/.claude/settings.json redirects Claude
#                      Code at a gateway. Settings and credentials both live in
#                      that directory, so it needs one interactive login first:
#                        mkdir -p ~/.cache/kompressor-claude-sweep/claude-home
#                        CLAUDE_CONFIG_DIR=~/.cache/kompressor-claude-sweep/claude-home claude
#                      then `/login`, quit, and re-run with CLAUDE_HOME set to it.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SELF_DIR/../.." && pwd)"
# shellcheck source=scripts/kompressor-sweep/common.sh
source "$SELF_DIR/common.sh"

KOMPRESSOR="${KOMPRESSOR:-$REPO/target/release/kompressor}"
CLAUDE="${CLAUDE:-$(command -v claude || echo "$HOME/.local/bin/claude")}"
PORT="${PORT:-8819}"
WORK="${WORK:-$HOME/.cache/kompressor-claude-sweep}"
CONFIG="$WORK/config.toml"
STATS="$WORK/stats.db"
REFS="$WORK/refs.db"
PROXY_LOG="$WORK/proxy.log"
PID_FILE="$WORK/proxy.pid"

say() { printf '\n== %s\n' "$*"; }

# Run Claude Code on the *subscription*, whatever the shell is carrying.
#
# `ANTHROPIC_API_KEY` in the environment silently takes precedence over the
# claude.ai login — Claude Code says so and carries on, and the sweep then
# measures an API-key session while reporting itself as a subscription test.
# Worse, a key belonging to a gateway rather than to Anthropic answers the
# first request with `400 could not auto resolve a provider`, nothing reaches
# the engine, and the run ends with zero events and no obvious cause.
#
# Unset for the child only; the user's shell keeps whatever it had.
claude_env() {
  local cleared=""
  [ -n "${ANTHROPIC_API_KEY:-}" ]        && cleared="$cleared ANTHROPIC_API_KEY"
  [ -n "${ANTHROPIC_AUTH_TOKEN:-}" ]     && cleared="$cleared ANTHROPIC_AUTH_TOKEN"
  [ -n "${CLAUDE_CODE_USE_BEDROCK:-}" ]  && cleared="$cleared CLAUDE_CODE_USE_BEDROCK"
  [ -n "${CLAUDE_CODE_USE_VERTEX:-}" ]   && cleared="$cleared CLAUDE_CODE_USE_VERTEX"
  if [ -n "$cleared" ]; then
    printf '  unset for this session (they override the subscription login):%s\n' "$cleared" >&2
  fi
  # A config directory of our own, when one is offered. `~/.claude/settings.json`
  # can carry an `env` block that Claude Code applies itself — a gateway token
  # or base URL set there survives `env -u` and is not overridden by
  # `--settings`, so isolating the whole directory is the only way past it
  # that does not mean editing the user's configuration.
  local home=()
  [ -n "${CLAUDE_HOME:-}" ] && home=(CLAUDE_CONFIG_DIR="$CLAUDE_HOME")
  env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN \
      -u CLAUDE_CODE_USE_BEDROCK -u CLAUDE_CODE_USE_VERTEX \
      "${home[@]}" \
      ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" ENABLE_TOOL_SEARCH=true "$@"
}

# What the model said last, which is the only thing that answers the question.
# The token is in the transcript regardless — it is a line of the file it was
# told to read — so searching the whole log reports every run as a pass.
# The model's last words, out of the stream-json transcript.
#
# `-p` on its own prints only the final answer, which is tidy and useless: the
# rule-1 search check and any batching check have nothing to read, so a model
# that greps its way to the right name is indistinguishable from one that
# fetched the body back. `--output-format stream-json --verbose` emits every
# tool call, at the cost of having to dig the answer out again.
final_answer() {
  python3 - "$1" <<'FINAL_EOF'
import json, sys
last = ""
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if event.get("type") == "result" and isinstance(event.get("result"), str):
        last = event["result"]
    elif event.get("type") == "assistant":
        for block in event.get("message", {}).get("content", []):
            if block.get("type") == "text":
                last = block.get("text", "")
print(last[-4000:])
FINAL_EOF
}
die() { printf 'kompressor-sweep/claude: %s\n' "$*" >&2; exit 2; }

# ---------------------------------------------------------------------------
# the proxy
# ---------------------------------------------------------------------------

# A configuration of this sweep's own, never the user's.
#
# `~/.config/kompressor/config.toml` is a working Codex setup on most machines
# here: its upstreams are OpenAI and `chatgpt.com`, and Claude Code pointed at
# it 404s because no mount answers `/v1/messages`. Writing a second mount into
# it would also mean editing a file the user relies on, to run a test.
# $1, when given, is `retrieve_max_rounds`. It belongs to `[proxy]`, not to
# `[retrieve]`, and it has to be written *inside* that block: appending to the
# end of the file would land it in the `[[proxy.upstreams]]` table above, where
# it is an unknown field and the config will not load.
write_config() {
  mkdir -p "$WORK"
  local rounds_line=""
  [ -n "${1:-}" ] && rounds_line="retrieve_max_rounds = $1"
  cat > "$CONFIG" <<TOML
[server]
listen = "127.0.0.1:$PORT"

[engine]
mode = "recoverable"

[refs]
path = "$REFS"

[stats]
path = "$STATS"

[proxy]
compress_timeout = "10s"
$rounds_line
# Load-bearing, and the whole reason the first attempt at this measured 0.00%.
#
# Claude Code puts its \`cache_control\` breakpoint on the FINAL message, so a
# marker-derived floor freezes the entire conversation and nothing is left to
# compress. Every Anthropic run before this setting existed reported exactly
# 0.0% removed. Do not "simplify" this back to the default.
frozen_prefix = "first_seen"

[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/v1"
base_url = "https://api.anthropic.com"
api_family = "anthropic"
# The subscription's own OAuth token is what must reach Anthropic; this proxy
# never substitutes a key of its own. Unlike Codex, whose subscription traffic
# goes to a different host and needs \`[proxy.upstreams.subscription]\`, Claude
# Code uses the same host and path under a plan — only the credential differs.
forward_auth = true

# Claude Code talks to more than the Messages API. \`/api/hello\` is its
# connectivity probe and \`/api/oauth/*\` carries the subscription login, and
# with only a \`/v1\` mount both 404 — measured: a session's third request
# came back 404 from this proxy. \`passthrough\` because these are not
# Messages bodies: there is nothing to compress and parsing them as a
# conversation would be wrong.
#
# Backticks are escaped because this heredoc is unquoted — it has to expand
# \$PORT and the paths — so an unescaped one runs as a command and silently
# deletes the words between them from the generated file.
[[proxy.upstreams]]
name = "anthropic-api"
path_prefix = "/api"
base_url = "https://api.anthropic.com"
api_family = "passthrough"
forward_auth = true
TOML
}

# Stop by PID, never by name.
#
# The binary is often installed under a versioned name, and the kernel truncates
# `comm` to 15 characters — `kompressor-9a34c54` becomes `kompressor-9a34`, which
# `pkill -x kompressor` does not match. A survivor then holds the port, and every
# later "run" silently measures the stale process instead.
stop_proxy() {
  if [ -f "$PID_FILE" ]; then
    local pid; pid="$(cat "$PID_FILE")"
    if kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    rm -f "$PID_FILE"
  fi
}

start_proxy() {
  stop_proxy
  # The sidecars too: a stale `-wal` over a database that was deleted under it
  # is a `sqlite: disk I/O error` at startup, which reads exactly like "the run
  # measured nothing".
  rm -f "$STATS"* "$REFS"* "$PROXY_LOG"
  write_config
  "$KOMPRESSOR" check-config --config "$CONFIG" >/dev/null || die "the generated config does not load"
  "$KOMPRESSOR" serve --config "$CONFIG" >"$PROXY_LOG" 2>&1 &
  echo $! > "$PID_FILE"
  sleep 3
  kill -0 "$(cat "$PID_FILE")" 2>/dev/null || { sed 's/^/    /' "$PROXY_LOG"; die "the proxy did not start"; }
  echo "  proxy on 127.0.0.1:$PORT, config $CONFIG"
}

# Did anything actually reach the proxy during this stage?
#
# Without this a session that never got here produces a verdict that reads like
# an engine result: "answered correctly: no / FAIL" when the truth is that the
# request went somewhere else entirely and nothing was ever compressed.
#
# The usual cause is another auth source winning. `~/.claude/settings.json` can
# carry an `env` block, and a gateway token or base URL set there is applied by
# Claude Code itself — `env -u` on the shell cannot remove it, and neither can
# `--settings`, which does not override the user block. The giveaway in the log
# is `claude.ai connectors are disabled because ANTHROPIC_API_KEY or another
# auth source is set`, usually followed by an error from that gateway rather
# than from Anthropic.
reached_the_proxy() {
  [ "$(sweep_requests_since "$STATS" "$1")" -gt 0 ]
}

explain_bypass() {
  local log="$1"
  echo "  VOID — nothing reached the proxy, so this run says nothing about"
  echo "         compression or retrieval."
  if grep -q 'another auth source is set' "$log" 2>/dev/null; then
    echo "         Claude Code reports another auth source taking precedence."
  fi
  local settings="$HOME/.claude/settings.json"
  if [ -f "$settings" ] && grep -qE '"ANTHROPIC_(AUTH_TOKEN|API_KEY|BASE_URL)"' "$settings" 2>/dev/null; then
    echo "         $settings has an env block setting:"
    grep -oE '"ANTHROPIC_(AUTH_TOKEN|API_KEY|BASE_URL)"' "$settings" | sort -u | sed 's/^/           /'
    echo "         Claude Code applies those itself, so unsetting the shell does"
    echo "         not help, and --settings does not override them. Two ways past:"
    echo "           a) comment the env block out for the duration of the sweep, or"
    echo "           b) give the sweep a config directory of its own —"
    echo "                mkdir -p \$HOME/.cache/kompressor-claude-sweep/claude-home"
    echo "                CLAUDE_CONFIG_DIR=\$HOME/.cache/kompressor-claude-sweep/claude-home claude"
    echo "              log in there once with /login, then re-run with"
    echo "                CLAUDE_HOME=\$HOME/.cache/kompressor-claude-sweep/claude-home"
  fi
  echo "         Session output: $log"
}

# Handles the session was actually shown, as opposed to ones a dry run minted.
#
# `bughunt-measure.py` and the retrieval premise check both compress through
# the *API* surface to decide whether a fixture is elidable. That answers "can
# this be elided", not "was it". If the live conversation emitted no handle the
# model never saw a marker, so a verdict about retrieval is meaningless however
# it answered — and the cause is usually `frozen_prefix = "first_seen"` on a
# short session, where the first appearance of everything is the frozen prefix.
live_handles() { sweep_live_handles "$STATS" "$1"; }

explain_nothing_elided() {
  local live="$1" since="$2"
  echo "  VOID — the live session was shown $live handle(s), so there was no marker"
  echo "         to fetch and the answer cannot say anything about retrieval."
  echo "         The 'elided' counts above are a dry run through the API surface:"
  echo "         they say the fixture CAN be elided, not that it was."
  # Reported rather than guessed. An early version of this message blamed
  # frozen_prefix every time; the first run it fired on had been declined by
  # the code compressor instead, which is a different problem with a different
  # fix. The engine already records why, per message.
  echo "         What the live session actually did, by reason:"
  sqlite3 "$STATS" \
    "select '           ' || j.key || '  ' || sum(j.value)
     from optimization_events, json_each(optimization_events.skipped_by_reason) as j
     where surface='proxy' and time_ms >= $since
     group by j.key order by sum(j.value) desc limit 6;" 2>/dev/null
  echo "         Reading those:"
  echo "           protected:frozen under frozen_prefix=\"first_seen\" usually means"
  echo "           the provider was ALREADY serving those bytes from cache, so"
  echo "           rewriting them would invalidate the entry and force the whole"
  echo "           conversation to be billed again. The engine declined a 90%+"
  echo "           elision to avoid a cache miss, which is the right trade and not"
  echo "           a misconfiguration. Measured on a one-turn session: 2 tokens"
  echo "           billed at full price against 34,574 served from cache."
  echo "           declined:<type> is different — the payload reached the"
  echo "           compressor and it refused the shape, which no frozen_prefix"
  echo "           setting will change."
  echo "         Check which it is:"
  echo "           sqlite3 $STATS \\"
  echo "             'select prompt_tokens, cache_read_tokens from proxy_requests;'"
  echo "         Near-zero prompt_tokens means there was nothing to save: against a"
  echo "         client that caches this aggressively these two stages will mostly"
  echo "         VOID, and that is the honest result rather than a defect to tune."
}

# ---------------------------------------------------------------------------
# stages
# ---------------------------------------------------------------------------

stage_fixtures() {
  say "fixtures (free)"
  local codex="$SELF_DIR/codex.sh"
  [ -x "$codex" ] || die "the fixture builder is missing: $codex"
  mkdir -p "$WORK"
  write_config
  KOMPRESSOR="$KOMPRESSOR" SKIP_BUILD=1 WORK="$WORK" CONFIG="$CONFIG" \
    bash "$codex" fixtures
}

stage_sweep() {
  say "sweep (PAID: this spends Claude-plan quota)"
  echo "  16 payloads: log x2, json x2, source_code, tabular, config x3, html,"
  echo "  listing x2, search, diff, text x2 — one Claude session, one command each"
  [ -x "$CLAUDE" ] || die "Claude Code not found at $CLAUDE"
  [ -f "$WORK/prompt.txt" ] || die "run the fixtures stage first"
  start_proxy

  local model=()
  [ -n "${CLAUDE_SWEEP_MODEL:-}" ] && model=(--model "$CLAUDE_SWEEP_MODEL")
  local before; before=$(date +%s%3N)

  # `ENABLE_TOOL_SEARCH=true` is not optional. Claude Code stops deferring MCP
  # and system tool schemas the moment ANTHROPIC_BASE_URL is a custom host and
  # materialises every one of them into the context instead — so wrapping a
  # session to make the prompt smaller would make it larger. Same value
  # `kompressor wrap claude` sets; see the agent catalogue.
  local session_status=0
  ( cd "$WORK/fixtures" && claude_env timeout 2400 \
    "$CLAUDE" -p "$(cat "$WORK/prompt.txt")" --permission-mode bypassPermissions \
      --output-format stream-json --verbose \
      "${model[@]}" < /dev/null ) > "$WORK/sweep-out.txt" 2>&1 || session_status=$?

  sleep 5
  if ! reached_the_proxy "$before"; then
    explain_bypass "$WORK/sweep-out.txt"
    stop_proxy
    return 1
  fi
  if ! python3 "$SWEEP_LIB/session-status.py" "$WORK/sweep-out.txt" "$session_status"; then
    echo "  ERROR — Claude session did not complete."
    return 1
  fi
  local results chars
  read -r results chars < <(python3 "$SWEEP_LIB/claude-session.py" "$WORK/sweep-out.txt" --count)
  if [ "$chars" -lt 100000 ]; then
    echo "  INCONCLUSIVE — tool results carried only $chars chars; the sweep was not fully exercised."
    return 1
  fi
  if grep -q 'SWEEP DONE' <<<"$(final_answer "$WORK/sweep-out.txt")"; then
    echo "  session finished all 16 commands"
  else
    echo "  !! the session did not reach 'SWEEP DONE' — see $WORK/sweep-out.txt"
    return 1
  fi
  # The stats writer batches on an interval; reading before it flushes reports
  # zero whatever happened.
  sleep 5
  stop_proxy
}

stage_report() {
  say "compression and retrieval"
  python3 "$SWEEP_LIB/claude-report.py" report "$WORK" "$KOMPRESSOR"
}

# Preserve each stage before the next dedicated proxy replaces its databases.
run_stage() {
  local stage="$1" status=0 began transcript
  began=$(date +%s%3N)
  mkdir -p "$WORK/reports"
  case "$stage" in
    sweep) transcript="$WORK/sweep-out.txt" ;;
    retrieval) transcript="$WORK/retrieval/run.log" ;;
    bughunt) transcript="$WORK/bughunt-run.log" ;;
  esac
  rm -f "$transcript"
  ( "stage_$stage" ) 2>&1 | tee "$WORK/reports/$stage.log" || status=${PIPESTATUS[0]}
  stop_proxy
  if [ -f "$transcript" ]; then
    say "Claude session — $stage"
    python3 "$SWEEP_LIB/claude-session.py" "$transcript"
    if [ "$stage" = sweep ]; then
      say "per-payload breakdown"
      python3 "$SWEEP_LIB/claude-session.py" "$transcript" --table --analyse "$KOMPRESSOR" "$CONFIG"
    fi
  fi
  python3 "$SWEEP_LIB/claude-report.py" record "$WORK" "$stage" "$began" "$status"
  say "what the proxy did with it — $stage"
  if [ -f "$WORK/reports/$stage.db" ]; then
    python3 "$SWEEP_LIB/runstats.py" "$WORK/reports/$stage.db" "$began" --proxy-only
    say "stats by provider and content type — $stage (includes local premise checks)"
    "$KOMPRESSOR" stats by-provider --stats "$WORK/reports/$stage.db" --from "$began"
    "$KOMPRESSOR" stats by-type --stats "$WORK/reports/$stage.db" --from "$began"
    say "retrieval statistics — $stage"
    "$KOMPRESSOR" stats retrievals --stats "$WORK/reports/$stage.db" --from "$began"
  fi
  echo "  elapsed         $(( ($(date +%s%3N) - began) / 60000 ))m $(( (($(date +%s%3N) - began) / 1000) % 60 ))s"
  echo "  transcript      $transcript"
  return "$status"
}

# ---------------------------------------------------------------------------
# retrieval — can the model fetch back what was elided?
# ---------------------------------------------------------------------------

stage_retrieval() {
  say "retrieval (PAID) — can the model fetch back a planted token?"
  [ -x "$CLAUDE" ] || die "Claude Code not found at $CLAUDE"
  local dir="$WORK/retrieval"
  rm -rf "$dir"; mkdir -p "$dir"

  local token; token="ZX-$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')-QQ"
  echo "  planted token: $token"

  # Two decoys of the same shape, read BEFORE the real one.
  #
  # This is the lever bughunt found. The first substantial payload of a
  # conversation lands inside the prefix the provider has already cached, so
  # the engine correctly leaves it alone — and a stage that reads exactly one
  # file is therefore guaranteed to have it frozen. Every voided retrieval run
  # had `fixture reads: 1`; every passing bughunt run read more than one.
  # Spending two cheap reads first puts `ledger.py` beyond the floor, where it
  # compresses and the question actually requires the retrieve loop.
  #
  # The decoys carry their own tokens, which nothing asks about; the question
  # names `ledger.py` so the three identically-shaped files cannot be confused.
  local decoy
  for decoy in 1 2; do
    rm -rf "$dir/.decoy"; mkdir -p "$dir/.decoy"
    python3 "$SWEEP_LIB/ledger.py" "$dir/.decoy" "ZX-decoy$decoy-QQ" --compact
    mv "$dir/.decoy/ledger.py" "$dir/warmup_$decoy.py"
  done
  rm -rf "$dir/.decoy"
  python3 "$SWEEP_LIB/ledger.py" "$dir" "$token" --compact

  start_proxy
  # Counted as a delta by wall clock. The table already holds whatever the
  # sweep retrieved, and reporting the total here would credit this stage with
  # fetches it did not cause.
  local before; before=$(date +%s%3N)

  # Prove the premise before spending anything: the answer must actually be
  # elided and the elision must carry a handle. Asked of the engine through the
  # compress API, so it costs nothing.
  #
  # Framed as a plain-text tool result, which is how Claude Code delivers one —
  # not codex's `[{"type":"input_text"...}]` array. The framing decides routing,
  # so checking the bare file would be checking a different payload.
  local compressed
  compressed=$(python3 - "$dir/ledger.py" <<'FRAME' | curl -s -m 60 -X POST "http://127.0.0.1:$PORT/v1/compress" -H 'content-type: application/json' --data-binary @-
import json, sys
body = open(sys.argv[1]).read()
print(json.dumps({"messages": [{"role": "tool", "content": body}]}))
FRAME
)
  [ -n "$compressed" ] || { echo "  the compress API did not answer — cannot check the premise"; return 1; }
  if grep -q "$token" <<<"$compressed"; then
    echo "  PREMISE BROKEN: the token survived compression, so it can be read"
    echo "                  without retrieving and this run would prove nothing"
    return 1
  fi
  if ! grep -q 'ref:k_' <<<"$compressed"; then
    echo "  PREMISE BROKEN: the output carries no handle — nothing to retrieve"
    return 1
  fi
  echo "  premise holds: the token is elided and the output carries handles"

  # The file is deleted between reading it and being asked about it. Without
  # that an agent with a shell just greps for the answer, which never puts the
  # elided body into its context at all.
  # Two cheap turns before the fixture, which is what makes the test possible
  # at all under `frozen_prefix = "first_seen"`.
  #
  # That setting freezes as far as the provider's cache reached when the proxy
  # FIRST saw the conversation. A prompt that opens by reading the fixture puts
  # it inside that floor, so it is frozen on the one turn the model needs it
  # elided — measured, twice, as `protected:frozen` with zero handles and a
  # correct answer read straight out of the clear text. Establishing the floor
  # on a turn that carries nothing interesting leaves the fixture beyond it.
  cat > "$dir/prompt.txt" <<'PROMPT_EOF'
Do these steps in order.

1. Run: cat warmup_1.py
2. Run: cat warmup_2.py
3. Run: cat ledger.py
4. Run: rm ledger.py
5. Answer this question: in the file `ledger.py` that you read in step 3,
   inside the function `reconcile_17`, exactly one call
   to `compute(...)` is passed a quoted string where every other call is
   passed a number. What is that exact string? Reply with only that string.

Two rules, and the result is void if either is broken.

Read the file with the single `cat` in step 3 and nothing else. No grep, rg,
awk, sed, perl, head, tail, or a script that opens it and prints matching
lines — not before step 4 and not after. Searching reaches the file on disk and
returns a few short lines, which is not what this measures: the question is
whether what you were SENT can be recovered.

Do not read, grep or search Claude Code's own session files either — anything
under ~/.claude/ — nor any transcript or history of this conversation. Those
hold a verbatim copy of what you were sent, so recovering the answer from them
measures the filesystem rather than the retrieve loop.

After step 4 the file is gone, so do not try to read it again and do not
recreate it. Everything you need arrived in step 3. If part of what you received
was replaced by a marker saying it was omitted, fetch the original back with the
retrieval tool before you answer. The tool is offered to you by the proxy; look
for it rather than assuming it is absent. Do not guess and do not answer from
memory.

PROMPT_EOF

  local model=(); [ -n "${CLAUDE_SWEEP_MODEL:-}" ] && model=(--model "$CLAUDE_SWEEP_MODEL")
  local session_status=0
  ( cd "$dir" && claude_env timeout 900 "$CLAUDE" -p "$(cat "$dir/prompt.txt")" --permission-mode bypassPermissions \
      --output-format stream-json --verbose "${model[@]}" < /dev/null ) > "$dir/run.log" 2>&1 || session_status=$?
  sleep 5
  if ! reached_the_proxy "$before"; then
    explain_bypass "$dir/run.log"; stop_proxy; return 1
  fi
  stop_proxy

  say "retrieval result"
  if ! python3 "$SWEEP_LIB/session-status.py" "$dir/run.log" "$session_status"; then
    echo "  ERROR — Claude session did not complete; retrieval was not scored."
    return 1
  fi
  if python3 "$SWEEP_LIB/persisted-output.py" "$dir/run.log"; then
    echo "  VOID — Claude Code offloaded a fixture read to disk and sent only a preview."
    echo "         Reduce the fixture size; the proxy needs the full tool output."
    return 1
  fi
  # The same rule-1 check bughunt runs, which this stage lacked entirely. A
  # session once answered by running `awk` over Claude Code's own transcript
  # under ~/.claude/projects/ — the elided bytes are on disk there, so the
  # answer came back without the retrieve loop ever turning.
  local reads widest searched
  read -r reads widest searched < <(python3 "$SWEEP_LIB/reads.py" "$dir/run.log" 2>/dev/null || echo "0 0 0")
  if [ "${searched:-0}" -gt 0 ]; then
    echo "  VOID — the model searched instead of reading ($searched search call(s))."
    echo "         Whatever it answered, that answer did not come from a fetch."
    return 1
  fi
  local live; live=$(live_handles "$before")
  if [ "${live:-0}" -eq 0 ]; then explain_nothing_elided "$live" "$before"; return 1; fi
  local fetched returned planted answered reread
  fetched=$(sqlite3 "$STATS" "select count(*) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  # A fetch that returned nothing is not evidence: one codex run fetched a
  # range matching no line, got 0 characters back, and still answered — so
  # counting fetches alone would report a pass for an answer from elsewhere.
  returned=$(sqlite3 "$STATS" "select coalesce(sum(returned_chars),0) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  planted=$(sqlite3 "$REFS" "select count(*) from refs where cast(fragment as text) like '%$token%'" 2>/dev/null || echo 0)
  answered="no"; case "$(final_answer "$dir/run.log")" in *"$token"*) answered="yes" ;; esac
  reread="no"; [ -f "$dir/ledger.py" ] && reread="yes"

  echo "  retrievals recorded : $fetched"
  echo "  characters returned : $returned"
  echo "  elided in flight    : $live handles (live proxy traffic)"
  echo "  token was elided    : $([ "${planted:-0}" -gt 0 ] && echo yes || echo no)   (fragments holding it: $planted)"
  echo "  answered correctly  : $answered   (token $token)"
  echo "  file was recreated  : $reread"
  if [ "$answered" = yes ] && [ "${fetched:-0}" -gt 0 ] && [ "${returned:-0}" -gt 0 ]; then
    echo "  PASS — the body was elided, fetched back, and the answer came from it"
  elif [ "$answered" = yes ]; then
    echo "  INCONCLUSIVE — answered without a retrieval that returned anything;"
    echo "                 the answer came from somewhere other than the fetch"
  else
    echo "  FAIL — the planted token was not in the final answer"
  fi
}

# ---------------------------------------------------------------------------
# bughunt — the code compressor and the retrieve loop, across languages
# ---------------------------------------------------------------------------

stage_bughunt() {
  say "bughunt (PAID) — code compressor + retrieval, eight languages"
  [ -x "$CLAUDE" ] || die "Claude Code not found at $CLAUDE"
  local dir="$WORK/bughunt" langs="$WORK/bughunt-languages.json"
  rm -rf "$dir"; mkdir -p "$dir"
  python3 "$SWEEP_LIB/bughunt.py" "$dir" >/dev/null

  # Retrieval here is budgeted by TURN, and eight languages need more turns
  # than the shipped default of three allows.
  local rounds="${BUGHUNT_ROUNDS:-16}"
  echo "  retrieve_max_rounds for this stage: $rounds (shipped default is 3)"
  write_config "$rounds"
  stop_proxy
  rm -f "$STATS"* "$REFS"* "$PROXY_LOG"
  "$KOMPRESSOR" check-config --config "$CONFIG" >/dev/null || die "the bughunt config does not load"
  "$KOMPRESSOR" serve --config "$CONFIG" >"$PROXY_LOG" 2>&1 &
  echo $! > "$PID_FILE"; sleep 3
  kill -0 "$(cat "$PID_FILE")" 2>/dev/null || { sed 's/^/    /' "$PROXY_LOG"; die "the proxy did not start"; }

  local before; before=$(date +%s%3N)
  # How many languages actually elide their bug. A language whose body is left
  # in the clear needs no retrieval, so including it would dilute the result.
  local ok
  ok=$(python3 "$SWEEP_LIB/bughunt-measure.py" "$KOMPRESSOR" "$CONFIG" "$dir" "$langs")
  echo "  $ok language(s) ready"
  python3 "$SWEEP_LIB/bughunt-table.py" "$langs"
  python3 "$SWEEP_LIB/bughunt-table.py" "$langs" --by-type
  if [ "${ok:-0}" -lt 2 ]; then
    echo "  too few languages elide the bug; this would prove nothing"
    stop_proxy; return 1
  fi

  cat > "$WORK/bughunt-prompt.txt" <<'PROMPT_EOF'
Each file in this directory defines twelve near-identical functions. Exactly
one function reads one element past the end of its array. Every function ends
with the same line, so the fault is only visible part-way inside a body.

Read ALL eight files before you answer, even if you believe you have found the
fault in the first one. Then name the faulty function, with just the function
name, once, on its own line.

Reading every file is not busywork here. The first file you read arrives in the
clear; the ones after it are what this measures. Stopping early makes the run
prove nothing, and a run that proves nothing has to be paid for again.

Three rules, and the result is void if any is broken:

1. Read whole files only — `cat file`, or your file-reading tool. Do not search
   inside files. No grep, rg, sed, awk, perl, or scripts in python/node that
   open files and print matching lines. This measures whether elided bodies can
   be recovered, and a search sidesteps the thing being measured.

2. Read exactly ONE file per turn. Do not combine reads into a single script,
   cell or loop, and do not cat several files at once.

3. If a body has been replaced by a marker saying it was omitted, fetch the
   original back with the retrieval tool before answering. Do not guess from
   the names — all twelve are named alike on purpose — and do not answer from
   memory.

   Retrieve at most twelve bodies per turn, using several retrieval calls side
   by side. Work through one language per retrieval turn, then continue with
   the next language. There is enough budget for all eight languages; do not
   request every language in one giant batch.

4. Do not read, grep, awk or otherwise search Claude Code's own session files —
   anything under ~/.claude/ — and do not read any transcript or history of this
   conversation. Those hold a verbatim copy of what you were sent, so recovering
   the answer from them measures the filesystem rather than the retrieve loop. A
   run that does this is void.
PROMPT_EOF

  local model=(); [ -n "${CLAUDE_SWEEP_MODEL:-}" ] && model=(--model "$CLAUDE_SWEEP_MODEL")
  local session_status=0
  ( cd "$dir" && claude_env timeout 1800 "$CLAUDE" -p "$(cat "$WORK/bughunt-prompt.txt")" --permission-mode bypassPermissions \
      --output-format stream-json --verbose "${model[@]}" < /dev/null ) > "$WORK/bughunt-run.log" 2>&1 || session_status=$?
  sleep 5
  if ! reached_the_proxy "$before"; then
    explain_bypass "$WORK/bughunt-run.log"; stop_proxy; return 1
  fi
  stop_proxy

  say "bughunt result"
  if ! python3 "$SWEEP_LIB/session-status.py" "$WORK/bughunt-run.log" "$session_status"; then
    echo "  ERROR — Claude session did not complete; answer correctness was not scored."
    echo "          See $WORK/bughunt-run.log and $PROXY_LOG"
    return 1
  fi
  local wanted fetched returned answered onbug searched log="$WORK/bughunt-run.log"
  # Derived from the fixture's own BUGGY_INDEX and asserted present in all
  # eight files, so a renamed fixture fails loudly instead of reading every
  # correct answer as wrong.
  wanted=$(python3 "$SWEEP_LIB/bugname.py" "$SWEEP_LIB/bughunt.py" "$dir") || {
    echo "  PREMISE BROKEN: $wanted"; return 1; }
  fetched=$(sqlite3 "$STATS" "select count(*) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  returned=$(sqlite3 "$STATS" "select coalesce(sum(returned_chars),0) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  # Did a fragment that actually holds the faulty line come back? `fetched > 0`
  # only says the loop turned; this says it turned on the right body.
  onbug=$(sqlite3 "$STATS" \
    "attach '$REFS' as r;
     select count(*) from retrieval_events e join r.refs f on f.ref = substr(e.ref, 3)
     where e.time_ms >= $before and e.found = 1
       and (cast(f.fragment as text) like '%bound = size%'
            or cast(f.fragment as text) like '%bound := size%');" 2>/dev/null || echo 0)
  # Rules 1 and 2 were read out of the stream-json transcript above.
  # Case-insensitive, and only against the final answer: the name appears in
  # twelve signatures in every file, so searching the transcript matches the
  # fixture rather than the answer. Go exports it capitalised.
  answered="no"
  if grep -qiE "(^|[^a-z0-9_])${wanted}([^a-z0-9_]|$)" <<<"$(final_answer "$log")"; then
    answered="yes"
  fi

  local live reads widest
  live=$(live_handles "$before")
  read -r reads widest searched < <(python3 "$SWEEP_LIB/reads.py" "$log" 2>/dev/null || echo "0 0 0")
  echo "  languages elided    : $ok   (dry run; the live session saw $live handle(s))"
  echo "  fixture reads       : $reads   (most in one turn: $widest, searches: $searched)"
  if [ "${live:-0}" -eq 0 ]; then
    explain_nothing_elided "$live" "$before"
    # The read shape explains the rest of it. With nothing elided the file
    # arrives in the clear, and the eight fixtures share one faulty index — so
    # a single read answers the question and the model never looks further.
    # Measured: 2 Bash calls, 1 fixture read, 0 searches, answer correct.
    if [ "${reads:-0}" -le 1 ]; then
      echo "         It read $reads of the eight files. With nothing elided the first"
      echo "         one shows the fault in the clear, so there was no reason to"
      echo "         read on or to fetch anything — the question is answerable"
      echo "         without the retrieve loop whenever compression does not happen."
      echo "         This is the one lever that works: the FIRST file read lands"
      echo "         inside the cached prefix and is frozen, while later ones are"
      echo "         beyond it and do compress. Every passing run read more than"
      echo "         one file; every voided one stopped at the first. The prompt"
      echo "         now says to read all eight, so a short run is the model"
      echo "         declining that instruction rather than a setting to change."
    fi
    return 1
  fi
  echo "  retrievals recorded : $fetched"
  echo "  characters returned : $returned"
  echo "  faulty body fetched : $onbug"
  echo "  faulty function     : $wanted"
  echo "  answered correctly  : $answered"
  if [ "${searched:-0}" -gt 0 ]; then
    echo "  VOID — the model searched inside the files, so it never needed the"
    echo "         markers. Whatever it answered, this says nothing about retrieval."
  elif [ "${widest:-0}" -gt 1 ]; then
    echo "  VOID — the model read $widest files in one turn, so the engine saw one"
    echo "         mixed-language blob rather than eight payloads and had almost"
    echo "         nothing to elide. Rule 2 exists to stop this."
  elif [ "$answered" = yes ] && [ "${onbug:-0}" -gt 0 ]; then
    echo "  PASS — the body was elided, the model fetched back the one holding the"
    echo "         faulty line, and it named the function"
  elif [ "$answered" = yes ]; then
    echo "  INCONCLUSIVE — right answer, but no fetched fragment carried the faulty"
    echo "                 line; it may have reached it another way"
  else
    echo "  FAIL — did not name the faulty function (see $log)"
  fi
}

usage() {
  sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

trap stop_proxy EXIT

# Serialise runs that share the databases and saved reports.
mkdir -p "$WORK"
exec 9>"$WORK/.sweep.lock"
flock -n 9 || die "another Claude sweep is running in $WORK"

case "${1:-all}" in
  all|sweep|retrieval|bughunt)
    say "setup"
    echo "  agent           $("$CLAUDE" --version)"
    echo "  model requested ${CLAUDE_SWEEP_MODEL:-Claude default; actual model printed after each session}"
    echo "  binary          $KOMPRESSOR"
    echo "  work directory  $WORK"
    say "plan"
    if [ "${1:-all}" = all ]; then
      echo "  fixtures        build and measure payloads locally                  (free)"
      echo "  sweep           read the 16 payloads through the proxy              (paid)"
      echo "  retrieval       fetch back a planted token                          (paid)"
      echo "  bughunt         recover omitted function bodies in eight languages  (paid)"
      echo "  report          stage statistics, verdicts and HTML                 (free)"
    else
      echo "  $1, followed by saved statistics, verdicts and HTML"
    fi
    ;;
esac

case "${1:-all}" in
  fixtures)  stage_fixtures ;;
  sweep|retrieval|bughunt)
             status=0
             run_stage "$1" || status=$?
             stage_report
             exit "$status" ;;
  report)    stage_report ;;
  all)       began=$(date +%s)
             rm -f "$WORK/reports/manifest.json"
             stage_fixtures
             for stage in sweep retrieval bughunt; do
               run_stage "$stage" || echo "  ($stage did not complete; continuing)"
             done
             stage_report
             say "done in $(( ($(date +%s) - began) / 60 ))m $(( ($(date +%s) - began) % 60 ))s"
             ;;
  *)        usage; exit 2 ;;
esac
