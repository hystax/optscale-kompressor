#!/usr/bin/env bash
# Reproduce the opencode + kompressor compression test.
#
#   scripts/kompressor-sweep/opencode.sh all        # fixtures, sweep, retrieval, bughunt, report
#   scripts/kompressor-sweep/opencode.sh fixtures   # build the payload tree only (free)
#   scripts/kompressor-sweep/opencode.sh sweep      # the paid part: one opencode session
#   scripts/kompressor-sweep/opencode.sh report     # saved stage totals, verdicts + HTML (free)
#   scripts/kompressor-sweep/opencode.sh retrieval  # paid: can the model fetch back a planted token?
#   scripts/kompressor-sweep/opencode.sh bughunt    # paid: code compressor + retrieval, eight languages
#
# `sweep`, `retrieval` and `bughunt` spend whatever the chosen model costs: an
# OpenCode Go plan for `opencode-go/*`, your own API key for `openai/*` or
# `anthropic/*`. Everything else is local.
#
# The opencode third of the sweep, beside `codex.sh` and `claude.sh`. It shares
# their fixture tree, their verdicts and their report, and differs where
# opencode does: it is wired by configuration rather than a base-URL variable,
# so every session goes through `kompressor wrap opencode` itself — the path a
# user runs — and that is part of what each stage tests.
#
#   KOMPRESSOR=/path   the binary to measure (default: the repo's release build)
#   OPENCODE=/path     the opencode CLI                (default: $(command -v opencode))
#   PORT=8821          the dedicated proxy's port
#   WORK=/path         where fixtures, databases and logs go
#                      reports/<stage>.db keeps each stage's statistics;
#                      kompressor-opencode-report.html is the combined report
#   OPENCODE_SWEEP_MODEL=provider/model   passed to `opencode run -m`
#                      (default: anthropic/claude-haiku-5-5, on your Anthropic API key)
#   SKIP_PREFLIGHT=1   skip the one-word check, made with the proxy out of the
#                      path, that the model answers at all before paid stages
#
# opencode runs from a home of the sweep's own ($WORK/opencode-home), never
# from yours: your MCP servers, plugins and instructions stay out of the
# measured prompt, the sessions stay out of your history, and the background
# service your TUI and OpenCode Desktop share is never touched. Only the login
# is borrowed — see `seed_opencode_home`.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SELF_DIR/../.." && pwd)"
# shellcheck source=scripts/kompressor-sweep/common.sh
source "$SELF_DIR/common.sh"

KOMPRESSOR="${KOMPRESSOR:-$REPO/target/release/kompressor}"
OPENCODE="${OPENCODE:-$(command -v opencode || echo "$HOME/.opencode/bin/opencode")}"
PORT="${PORT:-8821}"
WORK="${WORK:-$HOME/.cache/kompressor-opencode-sweep}"
MODEL="${OPENCODE_SWEEP_MODEL:-opencode-go/glm-5.3-flash}"
CONFIG="$WORK/config.toml"
STATS="$WORK/stats.db"
REFS="$WORK/refs.db"
PROXY_LOG="$WORK/proxy.log"
PID_FILE="$WORK/proxy.pid"
OC_HOME="$WORK/opencode-home"
# Where your own opencode keeps its login. opencode 2.x stores credentials in
# the `credential` table of this database; `auth.json` beside it is a leftover
# of 1.x that 2.x no longer reads.
USER_OC_DB="${XDG_DATA_HOME:-$HOME/.local/share}/opencode/opencode.db"
# opencode cuts a tool result at 2,000 lines or 51,200 bytes, saves the rest to
# disk and sends the model the head — so the proxy would compress a stub and
# the sweep would report the stub. The fixtures are capped to fit under these,
# with room for the shell's own additions, as claude.sh does for Claude Code's
# `bashOutputMaxChars`.
FIXTURE_MAX_CHARS="${FIXTURE_MAX_CHARS:-120000}"
TOOL_OUTPUT='{"max_lines":100000,"max_bytes":262144}'

say() { printf '\n== %s\n' "$*"; }
die() { printf 'kompressor-sweep/opencode: %s\n' "$*" >&2; exit 2; }
session() { python3 "$SWEEP_LIB/opencode-session.py" "$@"; }

# The environment every opencode child runs in.
#
# `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` are removed rather than passed
# through: on opencode 2.2.4 either one makes that provider's models
# "unavailable" and the session sends nothing. `OPENCODE_CONFIG*` would layer a
# shell's configuration over the sweep's own. NO_PROXY keeps a proxy in the
# environment from intercepting the 127.0.0.1 base URLs, which is what broke
# the first Claude sweeps.
opencode_env() {
  local no_proxy="127.0.0.1,localhost,::1"
  [ -n "${NO_PROXY:-}" ] && no_proxy="$no_proxy,$NO_PROXY"
  env -u OPENAI_BASE_URL -u ANTHROPIC_BASE_URL \
      -u OPENCODE_CONFIG -u OPENCODE_CONFIG_DIR -u OPENCODE_CONFIG_CONTENT \
      XDG_CONFIG_HOME="$OC_HOME/config" XDG_DATA_HOME="$OC_HOME/data" \
      XDG_STATE_HOME="$OC_HOME/state" XDG_CACHE_HOME="$OC_HOME/cache" \
      OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_CLAUDE_CODE=1 \
      KOMPRESSOR_HOME="$WORK/kompressor-home" \
      PATH="$(dirname "$OPENCODE"):$PATH" \
      NO_PROXY="$no_proxy" no_proxy="$no_proxy" "$@"
}

# A home of the sweep's own, with your login copied in.
#
# The copy is of the `credential` rows alone, refreshed on every run so a
# re-login in your own opencode reaches the next sweep; nothing is printed.
# A symlink, which is how codex.sh borrows `auth.json`, cannot work here: the
# credentials share a database with sessions, and the sweep's sessions are
# exactly what must not land in yours.
seed_opencode_home() {
  mkdir -p "$OC_HOME"/{config/opencode,data,state,cache}
  chmod 700 "$OC_HOME"
  # The overlay `wrap` adds travels in OPENCODE_CONFIG_CONTENT and outranks
  # this file; it only has to carry what the sweep needs beyond routing.
  printf '{\n  "$schema": "https://opencode.ai/config.json",\n  "tool_output": %s\n}\n' \
    "$TOOL_OUTPUT" > "$OC_HOME/config/opencode/opencode.json"
  [ -f "$USER_OC_DB" ] || die "no opencode login at $USER_OC_DB — sign in with opencode first"
  local db="$OC_HOME/data/opencode/opencode.db"
  if ! sqlite3 "$db" "select 1 from credential limit 1" >/dev/null 2>&1; then
    # opencode creates its schema on first start. An unknown model fails before
    # any network call, so this costs nothing.
    ( cd "$OC_HOME" && opencode_env timeout 60 "$OPENCODE" run --standalone \
        -m kompressor-sweep/none "init" < /dev/null >/dev/null 2>&1 ) || true
  fi
  sqlite3 "$db" "attach 'file:$USER_OC_DB?mode=ro' as mine;
                 delete from credential;
                 insert into credential select * from mine.credential;" 2>/dev/null \
    || die "could not copy your opencode login into $db"
  local providers
  providers=$(sqlite3 "$db" "select group_concat(integration_id, ', ') from credential;")
  echo "  opencode home   $OC_HOME (login copied for: ${providers:-nothing})"
}

# The model's last words — the only thing that answers a question. The token
# and the function name are all over the transcript as fixture text, so
# searching the whole log reports every run as a pass.
final_answer() { session "$1" --answer; }

# One opencode session through `kompressor wrap opencode`.
#
# `--no-proxy` because this script starts its own proxy per stage: `wrap`
# adopts the listener and still does everything else it does for a user — the
# overlay, `--standalone`, refusing a provider you routed elsewhere. stdout is
# the JSON event stream; stderr is wrap's account of the session, kept beside
# it.
run_opencode() {
  local dir="$1" limit="$2" prompt="$3" out="$4" status=0
  ( cd "$dir" && opencode_env timeout "$limit" "$KOMPRESSOR" wrap opencode --no-proxy \
      --port "$PORT" --config "$CONFIG" -- \
      run --format json --auto -m "$MODEL" "$prompt" < /dev/null ) \
    > "$out" 2> "$out.stderr" || status=$?
  grep -E '^kompressor: (opencode providers|[0-9]+ request|.*left on its own)' "$out.stderr" \
    | sed 's/^/  /' || true
  return "$status"
}

# A configuration of this sweep's own, never the user's.
# $1, when given, is `retrieve_max_rounds`. It belongs to `[proxy]`, and has to
# be written inside that block: appended at the end it would land in the last
# `[[proxy.upstreams]]` table, where it is an unknown field.
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
# opencode's anthropic provider marks cache breakpoints the way Claude Code
# does, and under the default marker rule that freezes the whole conversation.
# Chat Completions and Responses carry no markers, so for openai and OpenCode
# Zen/Go this changes nothing. See claude.sh for the measurement.
frozen_prefix = "first_seen"

# The mounts \`kompressor wrap opencode\` adds when it starts a proxy itself.
# Anthropic first: /v1/messages is a strict extension of /v1.
[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/v1/messages"
base_url = "https://api.anthropic.com"
api_family = "anthropic"
forward_auth = true

[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "https://api.openai.com"
api_family = "openai"
forward_auth = true

[proxy.upstreams.subscription]
base_url = "https://chatgpt.com"
rewrite_prefix = "/backend-api/codex"

# OpenCode Zen (/zen/v1) and Go (/zen/go/v1), one host. The family is read off
# the path, so Chat Completions, Responses and Messages all route here.
[[proxy.upstreams]]
name = "opencode-zen"
path_prefix = "/zen"
base_url = "https://opencode.ai"
api_family = "auto"
forward_auth = true
TOML
}

# Stop by PID, never by name: see claude.sh for the 15-character `comm` trap.
stop_proxy() {
  if [ -f "$PID_FILE" ]; then
    local pid; pid="$(cat "$PID_FILE")"
    if kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    rm -f "$PID_FILE"
  fi
}

# The proxy talks to providers directly, whatever proxy the shell carries;
# `exec` so the recorded `$!` is kompressor's PID and not a subshell's.
serve_proxy() {
  exec env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy \
      -u ALL_PROXY -u all_proxy \
      "$KOMPRESSOR" serve --config "$CONFIG"
}

# $1, when given, is passed to write_config.
start_proxy() {
  stop_proxy
  # The sidecars too: a stale `-wal` over a deleted database is a disk I/O
  # error at startup, which reads exactly like "the run measured nothing".
  rm -f "$STATS"* "$REFS"* "$PROXY_LOG"
  write_config "${1:-}"
  "$KOMPRESSOR" check-config --config "$CONFIG" >/dev/null || die "the generated config does not load"
  serve_proxy >"$PROXY_LOG" 2>&1 &
  echo $! > "$PID_FILE"
  sleep 3
  kill -0 "$(cat "$PID_FILE")" 2>/dev/null || { sed 's/^/    /' "$PROXY_LOG"; die "the proxy did not start"; }
  echo "  proxy on 127.0.0.1:$PORT, config $CONFIG"
}

reached_the_proxy() {
  [ "$(sweep_requests_since "$STATS" "$1")" -gt 0 ]
}

# What to do about the error opencode reported. Each usual reason has a
# different fix and none of them is in the engine, so a verdict that stopped at
# "did not complete" would send the reader looking in the wrong place.
explain_failure() {
  local log="$1" said
  said="$(session "$log" --error)"
  [ -z "$said" ] && said="$(grep -m1 -E 'Error|error' "$log.stderr" 2>/dev/null || true)"
  [ -n "$said" ] && echo "         opencode said: $said"
  case "$said" in
    *"Model unavailable"*|*"not found"*)
      echo "         $MODEL is not a model this login can use. List them with"
      echo "           opencode models"
      echo "         and set OPENCODE_SWEEP_MODEL=provider/model." ;;
    *"expired"*)
      echo "         The ${MODEL%%/*} API key stored in your opencode login has expired."
      echo "         Signing in again with the same key will not renew it: create a new"
      echo "         key with the provider (OpenCode Console for opencode and opencode-go),"
      echo "         store it with opencode auth login, and the next sweep copies it."
      echo "         Or run on another provider: OPENCODE_SWEEP_MODEL=provider/model." ;;
    *"credential"*|*"API key"*|*"Unauthorized"*|*"401"*)
      echo "         The login for ${MODEL%%/*} was refused. The sweep copies your own"
      echo "         opencode login on every run, so fix it there — opencode auth login,"
      echo "         or /connect in the TUI — or pick another provider with"
      echo "         OPENCODE_SWEEP_MODEL." ;;
    *"usage limit"*|*"Insufficient balance"*|*"CreditsError"*)
      echo "         That provider has no quota left for this login. Pick a model on a"
      echo "         provider you pay for with OPENCODE_SWEEP_MODEL." ;;
  esac
  echo "         Session output: $log (stderr: $log.stderr)"
}

# One word to the model with the proxy out of the path, before anything paid.
#
# A login the provider refuses fails every stage the same way, and each stage
# then reports it after building its fixtures and starting a proxy — three
# ERRORs for one expired key, which reads like three problems. Asked directly,
# a refusal here is the provider's answer and nothing of ours: the overlay and
# `wrap` are not involved. Costs one short completion; SKIP_PREFLIGHT=1 skips it.
preflight() {
  [ "${SKIP_PREFLIGHT:-0}" = 1 ] && return 0
  local dir="$WORK/preflight" status=0
  mkdir -p "$dir"
  ( cd "$dir" && opencode_env timeout 120 "$OPENCODE" run --standalone --format json \
      -m "$MODEL" "Reply with the single word: pong" < /dev/null ) \
    > "$dir/run.jsonl" 2> "$dir/run.jsonl.stderr" || status=$?
  if session "$dir/run.jsonl" --status "$status" >/dev/null; then
    echo "  preflight       $MODEL answers with the proxy out of the path"
    return 0
  fi
  echo "  preflight       FAILED — $MODEL does not answer even without the proxy,"
  echo "                  so no stage could measure anything. Nothing paid was run."
  explain_failure "$dir/run.jsonl"
  return 1
}

explain_bypass() {
  echo "  VOID — nothing reached the proxy, so this run says nothing about"
  echo "         compression or retrieval."
  explain_failure "$1"
}

# The session reached the proxy and still did not finish: usually the
# provider refused it, which the proxy forwarded as it should.
explain_incomplete() {
  local log="$1" what="$2"
  echo "  ERROR — opencode session did not complete; $what"
  explain_failure "$log"
}

live_handles() { sweep_live_handles "$STATS" "$1"; }

explain_nothing_elided() {
  local live="$1" since="$2"
  echo "  VOID — the live session was shown $live handle(s), so there was no marker"
  echo "         to fetch and the answer cannot say anything about retrieval."
  echo "         The 'elided' counts above are a dry run through the API surface:"
  echo "         they say the fixture CAN be elided, not that it was."
  echo "         What the live session actually did, by reason:"
  sqlite3 "$STATS" \
    "select '           ' || j.key || '  ' || sum(j.value)
     from optimization_events, json_each(optimization_events.skipped_by_reason) as j
     where surface='proxy' and time_ms >= $since
     group by j.key order by sum(j.value) desc limit 6;" 2>/dev/null
  echo "         protected:frozen on an anthropic/* model is the provider's cache"
  echo "         being kept warm (see claude.sh, which explains it at length);"
  echo "         declined:<type> means the compressor refused the payload's shape."
}

# A result opencode cut short reached the proxy as a stub, so the numbers below
# would be the engine's work on a stub reported as the sweep.
truncated_void() {
  if session "$1" --truncated; then
    echo "  VOID — opencode cut a tool result short before the proxy saw it."
    echo "         A fixture is over tool_output ($TOOL_OUTPUT); see the breakdown."
    return 0
  fi
  return 1
}

stage_fixtures() {
  say "fixtures (free)"
  local codex="$SELF_DIR/codex.sh"
  [ -x "$codex" ] || die "the fixture builder is missing: $codex"
  mkdir -p "$WORK"
  write_config
  KOMPRESSOR="$KOMPRESSOR" SKIP_BUILD=1 WORK="$WORK" CONFIG="$CONFIG" \
    FIXTURE_MAX_CHARS="$FIXTURE_MAX_CHARS" bash "$codex" fixtures
}

stage_sweep() {
  say "sweep (PAID: $MODEL)"
  echo "  16 payloads: log x2, json x2, source_code, tabular, config x3, html,"
  echo "  listing x2, search, diff, text x2 — one opencode session, one command each"
  [ -x "$OPENCODE" ] || die "opencode not found at $OPENCODE"
  [ -f "$WORK/prompt.txt" ] || die "run the fixtures stage first"
  start_proxy
  local before session_status=0 out="$WORK/sweep-out.jsonl"
  before=$(date +%s%3N)
  run_opencode "$WORK/fixtures" 2400 "$(cat "$WORK/prompt.txt")" "$out" || session_status=$?

  sleep 5
  if ! reached_the_proxy "$before"; then
    explain_bypass "$out"; stop_proxy; return 1
  fi
  if ! session "$out" --status "$session_status" >/dev/null; then
    explain_incomplete "$out" "nothing was measured."
    return 1
  fi
  if truncated_void "$out"; then stop_proxy; return 1; fi
  local results chars
  read -r results chars < <(session "$out" --count)
  if [ "$chars" -lt 100000 ]; then
    echo "  INCONCLUSIVE — tool results carried only $chars chars; the sweep was not fully exercised."
    return 1
  fi
  if grep -q 'SWEEP DONE' <<<"$(final_answer "$out")"; then
    echo "  session finished all 16 commands"
  else
    echo "  !! the session did not reach 'SWEEP DONE' — see $out"
    return 1
  fi
  # The stats writer batches on an interval; reading before it flushes reports
  # zero whatever happened.
  sleep 5
  stop_proxy
}

stage_report() {
  say "compression and retrieval"
  SWEEP_AGENT=opencode python3 "$SWEEP_LIB/claude-report.py" report "$WORK" "$KOMPRESSOR"
}

# Preserve each stage before the next dedicated proxy replaces its databases.
run_stage() {
  local stage="$1" status=0 began transcript
  began=$(date +%s%3N)
  mkdir -p "$WORK/reports"
  case "$stage" in
    sweep) transcript="$WORK/sweep-out.jsonl" ;;
    retrieval) transcript="$WORK/retrieval/run.jsonl" ;;
    bughunt) transcript="$WORK/bughunt-run.jsonl" ;;
  esac
  rm -f "$transcript" "$transcript.stderr"
  ( "stage_$stage" ) 2>&1 | tee "$WORK/reports/$stage.log" || status=${PIPESTATUS[0]}
  stop_proxy
  # The next stage's start_proxy deletes $PROXY_LOG, and a failure verdict
  # points at it, so by the end of `all` it is always the wrong stage's log.
  [ -f "$PROXY_LOG" ] && cp "$PROXY_LOG" "$WORK/reports/$stage-proxy.log"
  if [ -f "$transcript" ]; then
    say "opencode session — $stage"
    session "$transcript"
    if [ "$stage" = sweep ] && [ "$(session "$transcript" --count | cut -d' ' -f1)" -gt 0 ]; then
      say "per-payload breakdown"
      session "$transcript" --table --analyse "$KOMPRESSOR" "$CONFIG"
    fi
  fi
  SWEEP_AGENT=opencode python3 "$SWEEP_LIB/claude-report.py" record "$WORK" "$stage" "$began" "$status"
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

stage_retrieval() {
  say "retrieval (PAID: $MODEL) — can the model fetch back a planted token?"
  [ -x "$OPENCODE" ] || die "opencode not found at $OPENCODE"
  local dir="$WORK/retrieval"
  rm -rf "$dir"; mkdir -p "$dir"

  local token; token="ZX-$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')-QQ"
  echo "  planted token: $token"

  # Two decoys of the same shape, read BEFORE the real one. Against a provider
  # that caches (anthropic/*) the first substantial payload lands inside the
  # frozen prefix and is left alone; claude.sh explains the measurement. For
  # the others they cost two cheap reads and change nothing.
  local decoy
  for decoy in 1 2; do
    rm -rf "$dir/.decoy"; mkdir -p "$dir/.decoy"
    python3 "$SWEEP_LIB/ledger.py" "$dir/.decoy" "ZX-decoy$decoy-QQ" --compact
    mv "$dir/.decoy/ledger.py" "$dir/warmup_$decoy.py"
  done
  rm -rf "$dir/.decoy"
  python3 "$SWEEP_LIB/ledger.py" "$dir" "$token" --compact

  start_proxy
  local before; before=$(date +%s%3N)

  # Prove the premise before spending anything: the answer must actually be
  # elided and the elision must carry a handle. Framed as a plain tool result,
  # which is what opencode's shell tool sends on every wire it speaks.
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

  # The file is deleted between reading it and being asked about it, or an
  # agent with a shell greps for the answer and never needs the elided body.
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

Do not read, query or search opencode's own storage either — any opencode.db,
anything under a directory named opencode or opencode-home — nor any
transcript or history of this conversation. Those hold a verbatim copy of what
you were sent, so recovering the answer from them measures the filesystem
rather than the retrieve loop.

After step 4 the file is gone, so do not try to read it again and do not
recreate it. Everything you need arrived in step 3. If part of what you received
was replaced by a marker saying it was omitted, fetch the original back with the
retrieval tool before you answer. The tool is offered to you by the proxy; look
for it rather than assuming it is absent. Do not guess and do not answer from
memory.

PROMPT_EOF

  local session_status=0 log="$dir/run.jsonl"
  run_opencode "$dir" 900 "$(cat "$dir/prompt.txt")" "$log" || session_status=$?
  sleep 5
  if ! reached_the_proxy "$before"; then
    explain_bypass "$log"; stop_proxy; return 1
  fi
  stop_proxy

  say "retrieval result"
  if ! session "$log" --status "$session_status" >/dev/null; then
    explain_incomplete "$log" "retrieval was not scored."
    return 1
  fi
  if truncated_void "$log"; then return 1; fi
  local reads widest searched
  read -r reads widest searched < <(session "$log" --reads 2>/dev/null || echo "0 0 0")
  if [ "${searched:-0}" -gt 0 ]; then
    echo "  VOID — the model searched instead of reading ($searched search call(s))."
    echo "         Whatever it answered, that answer did not come from a fetch."
    return 1
  fi
  local live; live=$(live_handles "$before")
  if [ "${live:-0}" -eq 0 ]; then explain_nothing_elided "$live" "$before"; return 1; fi
  local fetched returned planted answered reread
  fetched=$(sweep_retrievals_since "$STATS" "$before")
  # A fetch that returned nothing is not evidence; see common.sh.
  returned=$(sweep_returned_since "$STATS" "$before")
  planted=$(sqlite3 "$REFS" "select count(*) from refs where cast(fragment as text) like '%$token%'" 2>/dev/null || echo 0)
  answered="no"; case "$(final_answer "$log")" in *"$token"*) answered="yes" ;; esac
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

stage_bughunt() {
  say "bughunt (PAID: $MODEL) — code compressor + retrieval, eight languages"
  [ -x "$OPENCODE" ] || die "opencode not found at $OPENCODE"
  local dir="$WORK/bughunt" langs="$WORK/bughunt-languages.json"
  rm -rf "$dir"; mkdir -p "$dir"
  python3 "$SWEEP_LIB/bughunt.py" "$dir" >/dev/null

  # Retrieval is budgeted by TURN, and eight languages need more turns than
  # the shipped default of three allows.
  local rounds="${BUGHUNT_ROUNDS:-16}"
  echo "  retrieve_max_rounds for this stage: $rounds (shipped default is 3)"
  start_proxy "$rounds"

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

Reading every file is not busywork here. Stopping early makes the run prove
nothing, and a run that proves nothing has to be paid for again.

Four rules, and the result is void if any is broken:

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

4. Do not read, query or search opencode's own storage — any opencode.db,
   anything under a directory named opencode or opencode-home — and do not read
   any transcript or history of this conversation. Those hold a verbatim copy
   of what you were sent, so recovering the answer from them measures the
   filesystem rather than the retrieve loop. A run that does this is void.
PROMPT_EOF

  local session_status=0 log="$WORK/bughunt-run.jsonl"
  run_opencode "$dir" 1800 "$(cat "$WORK/bughunt-prompt.txt")" "$log" || session_status=$?
  sleep 5
  if ! reached_the_proxy "$before"; then
    explain_bypass "$log"; stop_proxy; return 1
  fi
  stop_proxy

  say "bughunt result"
  if ! session "$log" --status "$session_status" >/dev/null; then
    explain_incomplete "$log" "answer correctness was not scored."
    echo "         Proxy log: $PROXY_LOG"
    return 1
  fi
  local wanted fetched returned answered onbug searched
  # Derived from the fixture's own BUGGY_INDEX and asserted present in all
  # eight files, so a renamed fixture fails loudly instead of reading every
  # correct answer as wrong.
  wanted=$(python3 "$SWEEP_LIB/bugname.py" "$SWEEP_LIB/bughunt.py" "$dir") || {
    echo "  PREMISE BROKEN: $wanted"; return 1; }
  fetched=$(sweep_retrievals_since "$STATS" "$before")
  returned=$(sweep_returned_since "$STATS" "$before")
  onbug=$(sweep_bug_body_fetched "$STATS" "$REFS" "$before")
  # Only against the final answer: the name is in twelve signatures in every
  # file. Case-insensitive, because Go exports it capitalised.
  answered="no"
  if grep -qiE "(^|[^a-z0-9_])${wanted}([^a-z0-9_]|$)" <<<"$(final_answer "$log")"; then
    answered="yes"
  fi

  local live reads widest
  live=$(live_handles "$before")
  read -r reads widest searched < <(session "$log" --reads 2>/dev/null || echo "0 0 0")
  echo "  languages elided    : $ok   (dry run; the live session saw $live handle(s))"
  echo "  fixture reads       : $reads   (most in one turn: $widest, searches: $searched)"
  if [ "${live:-0}" -eq 0 ]; then
    explain_nothing_elided "$live" "$before"
    if [ "${reads:-0}" -le 1 ]; then
      echo "         It read $reads of the eight files. With nothing elided the first"
      echo "         one shows the fault in the clear, so there was no reason to read"
      echo "         on or to fetch anything."
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
  sed -n '2,35p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# Serialise runs that share the databases and saved reports.
mkdir -p "$WORK"
exec 9>"$WORK/.sweep.lock"
flock -n 9 || die "another opencode sweep is running in $WORK"

# Only once the lock is ours. Installed before it, a second invocation that
# lost the race died through this trap and killed the *running* sweep's proxy
# by the shared PID file: the live session then failed with ECONNREFUSED.
trap stop_proxy EXIT

case "${1:-all}" in
  all|sweep|retrieval|bughunt)
    [ -x "$KOMPRESSOR" ] || die "no kompressor binary at $KOMPRESSOR — run: make release"
    [ -x "$OPENCODE" ] || die "opencode not found at $OPENCODE"
    say "setup"
    echo "  agent           opencode $("$OPENCODE" --version 2>/dev/null | awk '{print $NF}')"
    echo "  model requested $MODEL   (OPENCODE_SWEEP_MODEL; the models that answered are in the report)"
    echo "  binary          $KOMPRESSOR"
    echo "  work directory  $WORK"
    seed_opencode_home
    preflight || exit 1
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
