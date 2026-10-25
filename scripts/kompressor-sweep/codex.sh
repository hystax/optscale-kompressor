#!/usr/bin/env bash
# Reproduce the codex + kompressor + ChatGPT-subscription compression test.
#
#   scripts/kompressor-sweep/codex.sh all        # everything: reset, fixtures, sweep, report
#   scripts/kompressor-sweep/codex.sh fixtures   # build the payload tree only (free)
#   scripts/kompressor-sweep/codex.sh sweep      # the paid part: one codex session
#   scripts/kompressor-sweep/codex.sh report     # stats + HTML report + cost estimate
#   scripts/kompressor-sweep/codex.sh analyse    # per-payload engine-direct breakdown (free)
#   scripts/kompressor-sweep/codex.sh retrieval  # paid: does the model fetch back what was elided?
#   scripts/kompressor-sweep/codex.sh bughunt    # paid: code compressor + retrieval, in eight languages
#
# `sweep`, `retrieval` and `bughunt` spend ChatGPT-plan quota. Everything else is local.
#
# Every run builds this tree and installs it to `$KOMPRESSOR` first, so a sweep
# measures the code it ships beside rather than whatever was installed last.
# The build is incremental and a no-op costs about a third of a second.
#   SKIP_BUILD=1 ...        use the installed binary as-is
#   KOMPRESSOR=/path ...    name a binary; it is then never rebuilt
#   WEBSOCKET=1 ...         relay codex's websocket instead of letting it fall
#                           back to HTTPS. Off by default. Exercises the relay;
#                           it does not measure savings, because a relayed
#                           session reports no usage at all.
#   KOMPRESSOR_WS_CAPTURE=/path   where relayed frames are written.
#                           Defaults to $WORK/kompressor-ws-frames.jsonl.
#                           The file appears only when the relay is on,
#                           and `report` counts what it holds.
#   KOMPRESSOR_SWEEP_MODEL=name   the model every paid stage asks for.
#                           Defaults to whatever ~/.codex/config.toml pins, so
#                           the usual run is unchanged. Passed as `codex exec
#                           -m`, which pins it for the run rather than reading
#                           it back afterwards: an edit to the global config
#                           between two sweeps otherwise redirects the second
#                           one silently, and the only record is a different
#                           model name in the verdict.
set -euo pipefail

# Shared with `claude.sh`: where the payload builders live, and the checks
# that decide whether a paid run proved anything.
# shellcheck source=scripts/kompressor-sweep/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

KOMPRESSOR_OVERRIDDEN="${KOMPRESSOR:+yes}"
KOMPRESSOR="${KOMPRESSOR:-$HOME/.local/bin/kompressor}"
CODEX="${CODEX:-$HOME/.local/bin/codex}"
CONFIG="${CONFIG:-$HOME/.config/kompressor/config.toml}"
STATS="${STATS:-$HOME/.local/share/kompressor/stats.db}"
REFS="${REFS:-$HOME/.local/share/kompressor/refs.db}"
WORK="${WORK:-$HOME/.cache/kompressor-sweep}"
PORT="${PORT:-8817}"
# Relay codex's websocket instead of letting it fall back to HTTPS.
#
# Off by default *here*, which since the proxy's own default moved to on is no
# longer the same thing as not mentioning it. This script pins the setting in
# both directions — see `relay_wanted` — because a sweep that does not say
# which transport it measures measures whichever one the installed config
# happened to pick, and reports the result as if it had chosen.
#
# Off: codex tries a websocket, gets a 405 from a proxy that is not relaying,
# falls back to HTTPS and is compressed there, where the provider reports what
# it billed. On: codex keeps the socket, `response.create` frames are still
# compressed, but the relay never sees a `response.completed`, so the token
# counts never arrive. On one sweep that took the codex path from 1,768,634
# tokens saved to zero — measured before the relay compressed anything, so read
# it as the shape of the risk and not as today's number.
#
# Use it to exercise the relay, not to measure savings: every stage that reads
# usage reports nothing, which the report already explains as NULL rather than
# counting it as zero.
WEBSOCKET="${WEBSOCKET:-0}"
# Relayed websocket frames, kept with the run. Written only when the relay is
# on; an explicit KOMPRESSOR_WS_CAPTURE still wins, for a run that wants it
# somewhere specific.
FRAMES="${KOMPRESSOR_WS_CAPTURE:-$WORK/kompressor-ws-frames.jsonl}"
# What the running proxy was started with, written by `start_proxy`. Read by
# `ensure_proxy` to decide whether the listener on $PORT can be adopted or has
# to be restarted; see `proxy_env_matches`.
PROXY_ENV="$WORK/proxy-env"
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The tree this script ships in, so a run measures the code it sits next to
# rather than whatever binary happens to be installed. Empty when the script
# has been copied somewhere without its repo, which `build_kompressor` treats
# as "use what is installed".
REPO="$(cd "$SELF_DIR/../.." 2>/dev/null && pwd || true)"
[ -f "$REPO/Cargo.toml" ] || REPO=""

FIX="$WORK/fixtures"
PROMPT="$WORK/prompt.txt"
LOG="$WORK/sweep.log"
VERDICTS="$WORK/verdicts.json"

# The newest codex session file, without a pipe.
#
# `ls -t ... | head -1` looks harmless and is not: `head` exits after the first
# line while `ls` is still writing the rest, `ls` takes SIGPIPE, and under
# `set -o pipefail` that is a failed pipeline which `set -e` then acts on. The
# run stops dead — after the sweep, with the quota already spent and no report.
# It grows more likely with every session file on disk, which is why it starts
# happening only once the directory fills up.
#
# With an argument, only a session newer than that file counts. Without the
# guard a stage whose codex never started measures whatever session is newest
# on disk — another run's, or the user's own desktop codex — and files its
# numbers as this run's. Observed 2026-10-02: `wrap` refused to edit
# ~/.codex/config.toml, codex never launched, and the sweep still reported
# "tool results: 2 carrying 57944 chars" from a session 38 minutes older than
# the run. An empty result is the honest answer and every caller checks for it.
newest_session() {
  local newest="" f since="${1:-}"
  for f in "$HOME"/.codex/sessions/*/*/*/*.jsonl; do
    [ -e "$f" ] || continue
    if [ -n "$since" ] && [ ! "$f" -nt "$since" ]; then continue; fi
    if [ -z "$newest" ] || [ "$f" -nt "$newest" ]; then newest="$f"; fi
  done
  printf '%s' "$newest"
}

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

# The model the newest session actually ran on.
#
# Read from the session rather than from `~/.codex/config.toml`, because the
# config says what was asked for and the session says what answered. A machine
# with no `model =` line gets whatever codex defaults to that week, and the two
# are not the same fact.
#
# This is not bookkeeping. The paid verdicts depend on it: a model that emits a
# sentence before its tool calls defeats the relay's interception and the bug
# hunt reads FAIL, while one that calls straight away reads PASS — the same
# kompressor, the same commit, opposite answers. Two hosts produced exactly
# that on 2026-09-26, `gpt-5.5` against `gpt-6-sol`, and nothing in the output
# said why. A verdict that does not name its model is not reproducible.
codex_model() {
  local f
  f="$(newest_session)"
  [ -n "$f" ] || { printf 'unknown'; return; }
  python3 -c "
import json, sys
model = ''
for line in open(sys.argv[1], errors='replace'):
    try:
        d = json.loads(line)
    except ValueError:
        continue
    if d.get('type') == 'turn_context':
        model = (d.get('payload') or {}).get('model') or ''
        if model:
            break
print(model or 'unknown')
" "$f" 2>/dev/null || printf 'unknown'
}

# `codex --version` without the `codex-cli ` prefix.
codex_version() { "$CODEX" --version 2>/dev/null | awk '{print $NF}'; }

# The model `~/.codex/config.toml` pins, empty when it pins none.
#
# What *will* run, as opposed to `codex_model`, which reads the newest session
# and so answers what *did*. Before the first session of a run those are
# different facts: the newest session is the previous run's, and printing its
# model at setup labelled a whole sweep with last night's model — `gpt-6-sol`
# in the banner, `gpt-6-astra` in both verdicts of the same output, measured
# 2026-09-29.
codex_model_pinned() {
  sed -nE 's/^[[:space:]]*model[[:space:]]*=[[:space:]]*"?([^"#]+)"?.*/\1/p' \
    "$HOME/.codex/config.toml" 2>/dev/null | head -1 | tr -d '[:space:]'
}

# The model every paid stage asks for, and the arguments that ask for it.
#
# Empty when nothing is pinned and nothing is overridden, which leaves codex to
# pick as it always did. Resolved once here rather than per stage so the three
# paid stages of one run cannot disagree about what they measured.
SWEEP_MODEL="${KOMPRESSOR_SWEEP_MODEL:-$(codex_model_pinned)}"
MODEL_ARGS=()
if [ -n "$SWEEP_MODEL" ]; then
  MODEL_ARGS=(-m "$SWEEP_MODEL")
fi

# Report an agent run that ended non-zero, saying which of the two things
# happened.
#
# They need different answers and used to get the same one. `wrap` edits
# ~/.codex/config.toml before it launches anything and refuses when a key it
# needs is already assigned: codex never runs, no quota is spent, and the fix
# is to the config file. But codex can also start and then fail -- a model the
# account may not use, an expired token, a sandbox refusal -- and then the
# config is innocent, requests may already have been billed, and the reason is
# in the log rather than in wrap's banner.
#
# The old report assumed the first case always. It printed "the agent never
# started" and then the log's first six lines, which are kompressor's own
# banner, and it asserted no quota was spent. Measured 2026-10-10: a model the
# ChatGPT plan refused for this CLI version was reported as a wrap failure, the
# actual message sat on the last line under seven websocket-fallback lines, and
# kompressor had logged nine requests.
#
# `kompressor: <binary> exited with status N` is wrap's own words for "the
# agent ran and failed", so that line is what separates the two.
agent_failed() {
  local stage="$1" log="$2" status="$3"
  echo
  if grep -q "exited with status" "$log" 2>/dev/null; then
    echo "  codex started and then failed (exit $status) -- the wrapping was fine."
    # The reason rather than the preamble. The websocket lines are dropped
    # first: with the relay off -- the default, and where the savings are --
    # codex always fails the upgrade, retries five times and falls back, so
    # four `Reconnecting` lines and seven connect failures sit between the real
    # error and the eye. Keeping them crowded the 2026-10-10 message out of a
    # five-line window. Shown anyway if filtering leaves nothing, because then
    # the websocket really is the complaint.
    local reasons
    reasons="$(grep -aE '^(ERROR|error)[: ]' "$log" 2>/dev/null \
      | grep -avE 'Reconnecting\.\.\.|failed to connect to websocket' || true)"
    [ -n "$reasons" ] || reasons="$(grep -aE '^(ERROR|error)[: ]' "$log" 2>/dev/null || true)"
    printf '%s\n' "$reasons" | tail -5 | sed 's/^/    /'
    echo "    --- last lines ---"
    tail -4 "$log" | sed 's/^/    /'
    echo "  quota may have been spent: \`$KOMPRESSOR stats\` has the request count."
    verdict "$stage" INCONCLUSIVE "reason=codex ran and exited $status"
  else
    echo "  the agent never started: \`kompressor wrap codex\` exited $status"
    echo "  wrap edits ~/.codex/config.toml before launching anything and refuses"
    echo "  when a key it needs is already assigned -- \`openai_base_url\`, set for"
    echo "  the desktop app, is the usual one."
    tail -6 "$log" | sed 's/^/    /'
    echo "  no quota was spent and nothing was measured."
    verdict "$stage" INCONCLUSIVE "reason=wrap could not configure codex"
  fi
}

# What codex printed after its last token count: the model's final message,
# and nothing else.
#
# Every stage that checks an answer needs this, because the transcript also
# carries the tool output — which is the fixture, which is where the answer
# came from. Grepping the whole log for the expected string therefore matches
# the question's own material and reports any run that read the file as
# correct. The retrieval stage did exactly that and filed a wrong answer
# ("no audit_checksum is assigned in reconcile_17") as right, on the strength
# of the planted token appearing in the `cat` output — measured 2026-09-29.
# Shared rather than copied, because that check and the bug hunt's had already
# drifted apart once.
final_answer() {
  tr -d '\r' < "$1" | awk '
    /^tokens used$/ {buf = ""; skip = 1; next}
    skip            {skip = 0; next}
    /^kompressor: / {next}
                    {buf = buf $0 "\n"}
    END             {printf "%s", buf}'
}

# Write the configuration this script measures against.
#
# Called only when the file is absent, and it never touches one that exists:
# the config is the user's, and a sweep that silently rewrote it would change
# what every later run measures.
#
# It is written rather than printed for the same reason the codex symlink is
# made rather than described — this is a setup step, and a harness that stops
# to dictate a file it could have created is just friction. The values are not
# defaults: `max_ref_bytes` is raised because kompressor's shipped 64 KiB drops
# recoverability on exactly the payloads worth compressing, and the subscription
# block is what routes a ChatGPT-plan caller to chatgpt.com instead of 404ing
# against api.openai.com.
write_default_config() {
  local dir; dir=$(dirname "$CONFIG")
  mkdir -p "$dir" "$(dirname "$REFS")" "$(dirname "$STATS")"
  cat > "$CONFIG" <<EOF
# Written by kompressor-sweep/codex.sh because none existed.
# Yours to edit; this script will not overwrite it again.

[server]
listen = "127.0.0.1:$PORT"

[engine]
mode = "recoverable"
# The 64 KiB default drops recoverability on exactly the payloads worth
# compressing: a fold or an elision over that size keeps no fragment.
max_ref_bytes = 1048576

[refs]
backend = "sqlite"
path = "$REFS"

[stats]
enabled = true
path = "$STATS"

[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "https://api.openai.com"
api_family = "openai"
forward_auth = true

# Where the same mount goes when the caller pays by subscription.
[proxy.upstreams.subscription]
base_url = "https://chatgpt.com"
rewrite_prefix = "/backend-api/codex"
EOF
  echo "  wrote $CONFIG (none existed)"
}

# Build the tree and install it where this run will call it from.
#
# A sweep exists to measure the current engine, and the failure it kept having
# was measuring a stale one: the binary under `~/.local/bin` is whatever was
# installed last, and nothing in a run notices that it is three commits old.
# The numbers look plausible, which is what makes it expensive — a fixture
# table and a cost estimate that describe code nobody is running.
#
# So every run builds first. `cargo build` is incremental and costs about a
# second when nothing changed, which is cheaper than one wrong sweep. The
# install only happens when the bytes differ, so an unchanged tree does not
# disturb a binary another process may be running.
#
# Skipped when `$KOMPRESSOR` was overridden, because then the caller has named
# a binary on purpose and replacing it would be rude; when there is no repo
# beside the script; and when `SKIP_BUILD=1`.
build_kompressor() {
  if [ -n "${SKIP_BUILD:-}" ]; then
    echo "  SKIP_BUILD set — using $KOMPRESSOR as installed"
    return 0
  fi
  if [ -n "${KOMPRESSOR_OVERRIDDEN:-}" ]; then
    echo "  KOMPRESSOR was set explicitly — not rebuilding it"
    return 0
  fi
  if [ -z "$REPO" ]; then
    echo "  no source tree beside this script — using $KOMPRESSOR as installed"
    return 0
  fi

  echo "  building $REPO"
  if ! ( cd "$REPO" && cargo build --release -p kompressor-cli ) > "$WORK/build.log" 2>&1; then
    echo "  BUILD FAILED — $WORK/build.log"
    tail -20 "$WORK/build.log" | sed 's/^/    /'
    # Refuse rather than fall back. A sweep that quietly measured the previous
    # binary after a failed build is the exact confusion this stage removes.
    exit 1
  fi

  local built="$REPO/target/release/kompressor"
  if [ ! -x "$built" ]; then
    echo "  build reported success but $built is missing"
    exit 1
  fi
  if [ -x "$KOMPRESSOR" ] && cmp -s "$built" "$KOMPRESSOR"; then
    echo "  $KOMPRESSOR is already the current build"
  else
    # Written beside the target and renamed over it, never installed in place.
    # `install` writes *through* the existing inode (verified: the inode number
    # is unchanged), so a proxy already running from this path would have its
    # own executable image rewritten underneath it. `mv` within one directory
    # is a rename: the running process keeps the old inode until it exits and
    # the next start picks up the new one, which is `reset`'s job.
    mkdir -p "$(dirname "$KOMPRESSOR")"
    install -m755 "$built" "$KOMPRESSOR.new.$$"
    mv -f "$KOMPRESSOR.new.$$" "$KOMPRESSOR"
    echo "  installed $("$KOMPRESSOR" version | awk 'NR==1') -> $KOMPRESSOR"
  fi
}

# --------------------------------------------------------------------- setup
setup() {
  say "setup"
  mkdir -p "$WORK" "$(dirname "$STATS")" "$HOME/.local/bin"
  # codex is not on PATH when it comes from the ChatGPT desktop app
  [ -e "$CODEX" ] || ln -sf /usr/lib/chatgpt/resources/codex "$CODEX"
  build_kompressor
  "$KOMPRESSOR" version | awk 'NR==1'   # not `head -1`: see newest_session
  "$CODEX" --version
  # Printed next to the version because it is the other half of "what ran", and
  # the one that decides whether the paid stages can pass at all.
  # Only the pinned value is knowable here; codex records the model when it
  # runs, and the verdicts below name the one that actually answered.
  if [ -n "$SWEEP_MODEL" ]; then
    local source="pinned in ~/.codex/config.toml"
    if [ -n "${KOMPRESSOR_SWEEP_MODEL:-}" ]; then
      source="KOMPRESSOR_SWEEP_MODEL"
    fi
    echo "  codex model: $SWEEP_MODEL ($source, passed as \`codex exec -m\`)"
  else
    echo "  codex model: codex default, resolved when the first session runs"
    echo "               (nothing pinned and no KOMPRESSOR_SWEEP_MODEL;"
    echo "                each paid verdict records what answered)"
  fi
  # Checked here because nothing else notices it fails. `start_proxy` passes no
  # `--config` and falls back to kompressor's built-in defaults, so the proxy
  # comes up healthy on a machine with no config file at all — while every
  # `compress`/`detect` call in the script passes `--config "$CONFIG"` and
  # errors out. The stages then report "0 language(s) ready" or an empty
  # fixture table, which reads as an engine fault and is a missing file.
  [ -f "$CONFIG" ] || write_default_config
  # codex's login lives here. Without it `wrap` reads no `auth_mode`, falls back
  # to the API-key block, and codex refuses with `Missing environment variable:
  # OPENAI_API_KEY` — which reads like a kompressor fault and is not one.
  if [ ! -f "$HOME/.codex/auth.json" ]; then
    echo "no ~/.codex/auth.json — codex is not logged in."
    echo "Sign in first (browser flow):  codex login"
    echo "Then re-run. Without it this measures nothing."
    exit 1
  fi
  python3 -c "import json,pathlib,sys; m=json.loads((pathlib.Path.home()/'.codex/auth.json').read_text()).get('auth_mode'); print(f'  codex auth_mode: {m}'); sys.exit(0 if m else 1)" \
    || echo "  (auth.json has no auth_mode — wrap will use the API-key block)"
  # Start one if none is listening, rather than refusing. Telling the user to
  # run the command this script is about to run anyway made `all` impossible on
  # a machine with no proxy already up — which is every clean machine, and `all`
  # is the documented entry point. `reset` kills whatever is listening and
  # starts its own two seconds later, so the old check was not even protecting
  # the stage that follows it; it only blocked `retrieval` and `bughunt`, which
  # genuinely need a proxy and had nothing to start one.
  if curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    # Adopted only if it is the binary we just installed. `build_kompressor`
    # renames the new build over the old path on purpose, so a proxy started
    # before it keeps running from the old inode — healthy, answering, and
    # the wrong engine. Every stage would then measure code that is not in the
    # tree while the header printed the new version, which is the exact
    # confusion the build step was added to end.
    if ! proxy_is_current; then
      echo "  proxy on $PORT is running an older binary — restarting it"
      restart_proxy || exit 1
    elif ! proxy_env_matches; then
      echo "  proxy on $PORT was started with different settings — restarting it"
      echo "    wants: $(proxy_env_wanted | tr '\n' ' ')"
      echo "    has:   $(cat "$PROXY_ENV" 2>/dev/null | tr '\n' ' ' || echo 'unknown (started outside this script)')"
      restart_proxy || exit 1
    else
      echo "  proxy already up on $PORT"
    fi
  else
    echo "  no proxy on $PORT — starting one"
    start_proxy
    wait_for_proxy || {
      echo "proxy did not come up on $PORT; see $WORK/serve.log"
      exit 1
    }
    echo "  proxy up on $PORT"
  fi
}

# Is the listener on $PORT running the binary at $KOMPRESSOR?
#
# Compared by inode rather than by path: the install is a rename, so the old
# executable keeps the same *path* while being a different file. `/proc/pid/exe`
# follows to the inode the process actually mapped, which is what settles it.
# Answers false when there is no listener, when /proc is not readable, or when
# either side cannot be stat'd — all cases where "restart it" is the safe move.
proxy_is_current() {
  local pid running installed
  pid=$(ss -lptnH "sport = :$PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)
  [ -n "$pid" ] || return 1
  running=$(stat -Lc '%i' "/proc/$pid/exe" 2>/dev/null || true)
  installed=$(stat -Lc '%i' "$KOMPRESSOR" 2>/dev/null || true)
  [ -n "$running" ] && [ -n "$installed" ] && [ "$running" = "$installed" ]
}

# Was the listener on $PORT started with the environment this run wants?
#
# The inode check above answers "which binary"; this answers "which settings",
# and they fail differently. A proxy adopted with the wrong environment is
# healthy, current, and silently measuring something else: `WEBSOCKET=1`
# against a listener started without the relay runs the whole sweep over HTTPS
# and reports savings for a transport it never touched, and `bughunt` leaves
# its raised `RETRIEVE_MAX_ROUNDS=16` behind for every later stage.
#
# Measured, not hypothetical: the frame capture added on 2026-10-09 could not
# reach an adopted proxy at all, so the first relayed sweep after it still
# wrote no frames. The proxy had been up for eighty minutes.
#
# Compared against a marker `start_proxy` writes rather than against
# /proc/<pid>/environ, which holds the upstream API key. False when the marker
# is missing, which covers a proxy started by hand or by an older copy of this
# script — both cases where restarting is the answer.
#
# Not pid-stamped: the marker is written before `setsid --fork`, which hands
# back no pid to record. A kompressor of the same inode started by hand on
# $PORT while a marker from an earlier run still sits here would be adopted
# on that marker's word. Narrow, and it degrades to the behaviour this
# replaced.
proxy_env_matches() {
  [ -f "$PROXY_ENV" ] || return 1
  [ "$(cat "$PROXY_ENV")" = "$(proxy_env_wanted)" ]
}

# The marker `start_proxy` would write for a plain run of this stage — no
# caller overrides, which is every stage except `bughunt`.
proxy_env_wanted() {
  local effective
  effective="$(proxy_effective_config)"
  printf '%s\n' "KOMPRESSOR_WS_CAPTURE=$FRAMES"
  # The relay as the proxy will actually resolve it, not as this script asked
  # for it. `relay_wanted` now pins the override in both directions, so the two
  # agree -- but keep reading the resolved value rather than the override. It
  # was already wrong once the other way round: a run without `WEBSOCKET=1`,
  # against a config file carrying `websocket_relay = true`, restarted a
  # perfectly good proxy and printed a diff implying the relay was about to be
  # switched off. It stayed on, and the run relayed. The resolved configuration
  # is the only thing that answers "will this proxy behave as this run needs".
  echo "relay=$(relay_of "$effective")"
  # Everything else the proxy reads, as one digest over the *resolved*
  # configuration rather than over the file.
  #
  # The file and the ambient environment were hashed separately at first, and
  # both were the wrong thing to hash. `env` passes the whole environment
  # through, so a `KOMPRESSOR__*` override reaches the proxy without appearing
  # in the lines above; `--config` means the file is read once, at startup; and
  # hashing the file counts its comments, so editing a comment restarted a
  # working proxy for a change that could not affect it. `check-config` applies
  # the file and the environment the way `serve` does and prints what results,
  # so one digest over that output covers both inputs and ignores everything
  # that is not a setting.
  #
  # Secrets are redacted in that output, so a changed `auth.secret` does not
  # move the digest. This script does not use auth; a caller that does should
  # restart its own proxy.
  echo "effective-sha256=$(digest_of "$effective")"
}

# The resolved configuration, as the proxy will see it with this run's
# environment. Empty when the binary cannot be run.
proxy_effective_config() {
  env "KOMPRESSOR__PROXY__WEBSOCKET_RELAY=$(relay_wanted)" \
    "$KOMPRESSOR" check-config --config "$CONFIG" 2>/dev/null || true
}

# The relay this run wants, as a word the proxy parses.
#
# Named in both directions, deliberately. The proxy ships with the relay on, so
# leaving the variable out no longer means "no relay" — it means "whatever the
# installed config resolves to", which for `WEBSOCKET=0` is the opposite of
# what the stage is about to report. An unset override was safe only while the
# shipped default agreed with this script's; it no longer does.
relay_wanted() {
  if [ "$WEBSOCKET" = "1" ]; then echo true; else echo false; fi
}

# `true` or `false` from a resolved configuration; `unknown` when it is empty.
relay_of() {
  printf '%s\n' "$1" | grep -m1 '^websocket_relay' | tr -d ' ' | cut -d= -f2 \
    | grep -E '^(true|false)$' || echo unknown
}

# A digest of the resolved configuration. Falls back to the file when
# `check-config` produced nothing, so a change is still noticed when the only
# thing that can resolve it is unavailable.
digest_of() {
  if [ -n "$1" ]; then
    printf '%s' "$1" | sha256sum | cut -c1-16
  else
    echo "unresolved-$(proxy_config_digest)"
  fi
}

# The config file as the proxy read it, or `none` when there is no file.
proxy_config_digest() {
  if [ -f "$CONFIG" ]; then
    sha256sum "$CONFIG" | cut -c1-16
  else
    echo none
  fi
}

# Stop whatever holds $PORT and start a proxy from the current binary.
#
# The stores are left alone; this is not `reset`.
restart_proxy() {
  local pid
  pid=$(ss -lptnH "sport = :$PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)
  [ -n "$pid" ] && { kill "$pid"; sleep 2; }
  start_proxy
  wait_for_proxy || {
    echo "proxy did not come back on $PORT; see $WORK/serve.log"
    return 1
  }
  echo "  proxy up on $PORT ($("$KOMPRESSOR" version | awk 'NR==1'))"
}

# --------------------------------------------------------------------- reset
reset() {
  say "reset stats"
  local pid; pid=$(ss -lptnH "sport = :$PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)
  [ -n "$pid" ] && { kill "$pid"; sleep 2; }
  rm -f "$STATS"* "$REFS"* "$WORK/serve.log" "$FRAMES"
  start_proxy
  # Was a fixed `sleep 4`, which is both too long on a warm machine and too
  # short on a cold one — a slow start reported "reset" as done and left every
  # later stage talking to nothing.
  wait_for_proxy && echo "  restarted clean" || {
    echo "proxy did not come back on $PORT; see $WORK/serve.log"
    exit 1
  }
}

# ------------------------------------------------------------------ fixtures
fixtures() {
  say "fixtures"
  rm -rf "$FIX"; mkdir -p "$FIX"; cd "$FIX"
  python3 - <<'PY'
import json, os, random
random.seed(11)
for d in ('src','logs','data','data/many','docs'): os.makedirs(d, exist_ok=True)

# log ------------------------------------------------------------------ log
with open('logs/service.log','w') as f:
    for i in range(4000):
        lvl='ERROR' if i%500==0 else ('WARN' if i%97==0 else 'DEBUG')
        f.write(f"2026-09-22T10:{i//60%60:02d}:{i%60:02d}Z {lvl} [req-{i:05d}] "
                f"resolved route /api/v2/items/{i%7} in {i%40}ms cache={'hit' if i%3 else 'miss'}\n")

# build output --------------------------------------------------------- log
with open('logs/build.log','w') as f:
    for i in range(1200):
        f.write(f"Setting up libfoo{i%80}-dev:amd64 (1.{i%9}.{i%7}-1build2) ...\n")
        if i%50==0:
            f.write(f"src/module_{i%12}/file_{i}.c:{i%90}:12: warning: "
                    f"unused variable 'tmp{i}' [-Wunused-variable]\n")

# SMALL json — must stay under codex's ~40 KB truncation so it still parses
small=[{"id":i,"name":f"item-{i}","status":random.choice(["active","idle","failed"]),
        "score":round(random.random(),4),"team":f"team-{i%9}"} for i in range(260)]
open('data/small.json','w').write(json.dumps(small, indent=2))

# BIG json — codex truncates it mid-array, so it will NOT parse (known gap)
big=[{"id":i,"name":f"item-{i}","status":random.choice(["active","idle","failed"]),
      "tags":["alpha","beta"],"score":round(random.random(),4),
      "owner":{"team":f"team-{i%9}","email":f"u{i%40}@example.com"}} for i in range(1500)]
open('data/records.json','w').write(json.dumps(big, indent=2))

# source_code --------------------------------------------------- source_code
with open('src/engine.py','w') as f:
    f.write('"""Generated module."""\n\n')
    for n in range(45):
        f.write(f"def transform_{n}(records, options=None):\n    \"\"\"Pass {n}.\"\"\"\n")
        for k in range(30): f.write(f"    step_{k} = compute(records, {k}, offset={n*k})\n")
        f.write("    return records\n\n\n")

# tabular ---------------------------------------------------------- tabular
with open('data/metrics.csv','w') as f:
    f.write("date,region,product,units,revenue,margin\n")
    for i in range(2500):
        f.write(f"2026-0{i%9+1}-{i%28+1:02d},region-{i%6},prod-{i%30},"
                f"{i%400},{i*13%99999}.{i%100:02d},0.{i%90:02d}\n")

# config: yaml, toml, ini ------------------------------------------- config
with open('data/deploy.yaml','w') as f:
    for i in range(700):
        f.write(f"service_{i}:\n  image: registry.example.com/app:{i%40}.0.1\n"
                f"  replicas: {i%5+1}\n  port: {8000+i%200}\n  env: production\n")
with open('data/app.toml','w') as f:
    for i in range(600):
        f.write(f"[worker.pool_{i}]\nsize = {i%16+1}\ntimeout_ms = {1000+i%900}\n"
                f"retries = {i%5}\nendpoint = \"https://svc-{i%30}.internal:8443\"\n\n")
with open('data/server.ini','w') as f:
    for i in range(700):
        f.write(f"[section_{i}]\nhost=10.0.{i%255}.{i%99}\nport={9000+i%500}\n"
                f"enabled={'yes' if i%2 else 'no'}\nweight={i%100}\n")

# html ---------------------------------------------------------------- html
with open('data/report.html','w') as f:
    f.write("<!DOCTYPE html>\n<html>\n<head><title>Report</title></head>\n<body>\n")
    for i in range(900):
        f.write(f'  <div class="row" id="r{i}"><span class="k">metric {i}</span>'
                f'<span class="v">{i*7%1000}</span></div>\n')
    f.write("</body>\n</html>\n")

# listing fodder ---------------------------------------------------- listing
for i in range(400): open(f'data/many/asset_{i:04d}.bin','w').write('x'*(i%50+10))

# search fodder ------------------------------------------------------ search
for i in range(25):
    with open(f'src/mod_{i:02d}.py','w') as f:
        for k in range(120):
            f.write(f"# TODO(perf): revisit cache path {i}-{k}\n" if k%7==0
                    else f"value_{k} = {k} * {i}\n")

# carriage-return progress frames ------------------------------- redraw fold
with open('logs/progress.txt','w') as f:
    for job in range(60):
        f.write(''.join(f"  downloading package-{job:02d} {p:3d}%\r" for p in range(0,101,2))
                + f"  downloading package-{job:02d} done\n")

# prose --------------------------------------------------------------- text
with open('docs/NOTES.md','w') as f:
    para=("The collector keeps a bounded queue and drops the oldest event when the writer "
          "falls behind. This is deliberate: a stalled writer must never become backpressure "
          "on the request path. ")
    for i in range(240): f.write(f"## Note {i}\n\n{para}\n\n")

# prose with fenced code --------------------------------------------- mixed
with open('docs/GUIDE.md','w') as f:
    for i in range(80):
        f.write(f"### Step {i}\n\nRun the helper and check the result.\n\n```python\n")
        for k in range(26): f.write(f"    value_{k} = transform(records, {k}, offset={i*k})\n")
        f.write("```\n\n")
print("fixtures built")
PY
  # a real multi-file git diff --------------------------------------- diff
  git init -q .
  git add -A
  git -c user.email=t@t -c user.name=t commit -qm base
  python3 - <<'PY'
# A few lines across many files, which is what a real patch looks like and what
# the diff compressor is built for: it keeps the `diff_max_files` (8) most
# changed and elides the rest, then narrows the context around the surviving
# hunks.
#
# The previous fixture did the opposite -- three global `.replace()` calls that
# rewrote EVERY matching line in three big files -- and so exercised neither
# lever. There was nothing to drop (3 files, all kept under the cap of 8) and no
# context to narrow (every line was a change), so `crush_diff` correctly
# declined and the sweep reported `diff 0.00%` against a 176,766-char payload.
# That read as a defect in the compressor and was a property of the fixture.

# Three well-separated lines in each of the 25 search-fodder modules: 25 files,
# three hunks apiece, and plenty of untouched context between them for the kept
# files to narrow. Separated deliberately -- adjacent edits merge into one hunk
# and the patch stops looking like the refactor it is meant to imitate.
for i in range(25):
    path = f'src/mod_{i:02d}.py'
    s = open(path).read()
    for k in (3, 45, 90):
        s = s.replace(f'value_{k} = {k} * {i}', f'value_{k} = {k + 1} * {i}  # retuned', 1)
    open(path, 'w').write(s)

# And one line each in three files of other types, so the patch is not all
# Python. `count=1` matters: without it these rewrite the whole file again.
for path, old, new in [('src/engine.py', 'step_0 =', 'step_0 = adjusted  #'),
                       ('data/deploy.yaml', 'replicas: 1', 'replicas: 4'),
                       ('data/metrics.csv', 'region-0', 'region-9')]:
    s = open(path).read()
    open(path, 'w').write(s.replace(old, new, 1))
PY
  cat > "$PROMPT" <<'EOF'
You are exercising a compression proxy. Run each command below, exactly once, in this order.
Show the complete output of every command. Do not pipe to head/tail/wc, do not summarise, do
not skip any, do not stop early, and do not combine them into one command.
After the last one, reply with exactly: SWEEP DONE

1. cat logs/service.log
2. cat logs/build.log
3. cat data/small.json
4. cat data/records.json
5. cat src/engine.py
6. cat data/metrics.csv
7. cat data/deploy.yaml
8. cat data/app.toml
9. cat data/server.ini
10. cat data/report.html
11. ls -la data/many
12. grep -rn TODO src
13. git diff
14. cat logs/progress.txt
15. cat docs/NOTES.md
16. cat docs/GUIDE.md
EOF
  du -sh "$FIX"

  # What is about to be tested, and how compressible each payload is on its own.
  # Measured by the engine directly, so it is the ceiling: through codex these
  # same files arrive wrapped in tool-output framing, truncated at ~40 KB, and
  # mixed with conversation the proxy must leave alone.
  say "what the sweep will send"
  local list; list=$(sed -n 's/^[0-9]*\. cat //p' "$PROMPT" | tr '\n' ' ')
  # shellcheck disable=SC2086
  ( cd "$FIX" && python3 "$SWEEP_LIB/measure.py" \
      "$KOMPRESSOR" "$CONFIG" "$WORK/fixture-languages.json" $list >/dev/null )
  python3 "$SWEEP_LIB/bughunt-table.py" "$WORK/fixture-languages.json"
  python3 "$SWEEP_LIB/bughunt-table.py" "$WORK/fixture-languages.json" --by-type
  echo
  echo "  plus: ls -la data/many, grep -rn TODO src, git diff — listing, search and diff,"
  echo "  which have no single file to measure here."
}

# Every `wrap` below passes `--config "$CONFIG"`, and it is not optional.
# Without it `wrap` runs its own configuration discovery and points the agent at
# whatever `listen` *that* file names — the per-user default, not this run's.
# With PORT or CONFIG overridden the two diverge silently: the sweep starts its
# proxy on one port and sends codex to another, this run's database stays empty,
# and the report asks "did the agent reach the proxy?" while a `stats` stage
# reading the default config prints another proxy's traffic as this run's.
# Measured 2026-10-08, twice, before the divergence was spotted.
#
# A comment cannot sit inside a `\`-continued command, which is how the first
# attempt at writing this down fed `timeout` a `#` as its argument and killed
# the sweep stage outright.
# --------------------------------------------------------------------- sweep
sweep() {
  say "sweep — this spends ChatGPT-plan quota"
  local started started_s wrap_status=0
  started=$(date +%s%3N); started_s=$(date +%s)
  echo "  16 payloads: log x2, json x2, source_code, tabular, config x3, html,"
  echo "  listing x2, search, diff, text x2 — one codex session, one command each"
  cp ~/.codex/config.toml "$WORK/codex-config.bak"
  cd "$FIX"
  rm -f "$LOG"
  # Anything codex writes for this stage is newer than this marker.
  : > "$WORK/.sweep-started"
  # `wrap` edits ~/.codex/config.toml, runs codex, and puts the file back.
  # --no-proxy: use the server already running rather than starting another.
  # < /dev/null: codex exec hangs forever on "Reading additional input from stdin"
  #              when stdin is an open pipe.
  # `plugins."codex-app-tools@openai-bundled".enabled=false` turns off the
  # code-mode path. With it on, codex has been observed to answer the whole
  # prompt by running one empty cell — it never reads a file, so the sweep
  # measures nothing. Measured directly: with the plugin disabled the same
  # request executes the command and returns its real output.
  PATH="$HOME/.local/bin:$PATH" timeout 2400 \
    "$KOMPRESSOR" wrap codex --no-proxy --config "$CONFIG" -- \
      exec --skip-git-repo-check "${MODEL_ARGS[@]}" \
      -c 'plugins."codex-app-tools@openai-bundled".enabled=false' \
      "$(cat "$PROMPT")" < /dev/null > "$LOG" 2>&1 || wrap_status=$?
  tail -2 "$LOG"
  # Why it ended, and whether it ran at all, is `agent_failed`'s to say.
  if [ "${wrap_status:-0}" -ne 0 ]; then
    agent_failed sweep "$LOG" "$wrap_status"
    return 1
  fi
  diff -q ~/.codex/config.toml "$WORK/codex-config.bak" >/dev/null \
    && echo "codex config restored" \
    || echo "WARNING: codex config differs — run: $KOMPRESSOR unwrap codex"

  # Did the agent actually do the work? It can answer "SWEEP DONE" after running
  # one empty cell, and then every number below is a measurement of nothing.
  # Codex's own session log is the honest record: count the tool results it
  # received and how much text they carried.
  local sess counts
  sess=$(newest_session "$WORK/.sweep-started")
  if [ -z "$sess" ]; then
    echo "  codex wrote no session for this stage; there is nothing to measure."
    verdict sweep INCONCLUSIVE "reason=no session file newer than the stage start"
    return 1
  fi
  counts=$(python3 "$SWEEP_LIB/count.py" "$sess")
  set -- $counts
  echo "  tool results: $1 carrying $2 chars"
  # Bytes, not call count: under code mode the agent batches several commands
  # into one cell, so four calls carrying 175 KB is a real run while one call
  # carrying 144 bytes is an empty one.
  # A dead session and a code-mode empty cell both land here with no bytes, and
  # they need opposite responses: one is codex's login, the other is codex's
  # plugin. Told to disable the plugin for a 401, the reader changes the one
  # thing that was never wrong and runs again into the same wall — measured
  # 2026-09-26, twice.
  if session_died "$LOG"; then
    echo
    echo "  WARNING: the agent session failed upstream: $(session_death "$LOG")."
    echo "           It never read the files, so there is nothing to report and this"
    echo "           says nothing about compression. This is codex's own credential,"
    echo "           not the proxy — confirm with the proxy out of the path:"
    echo "             codex exec --skip-git-repo-check 'reply with just: OK'"
    echo "           If that fails too, sign in again: codex login"
    return 1
  fi
  if [ "${2:-0}" -lt 100000 ]; then
    echo
    echo "  WARNING: the agent did not run the sweep. It answered without reading the"
    echo "           files, so the numbers from 'report' would measure nothing. Codex"
    echo "           can take a code-mode path and execute an empty cell. Re-run, and"
    echo "           if it persists disable the code-mode plugin for the run:"
    echo "             -c 'plugins.\"codex-app-tools@openai-bundled\".enabled=false'"
    return 1
  fi
  say "what the proxy did with it"
  python3 "$SWEEP_LIB/runstats.py" "$STATS" "$started"
  echo "  elapsed         $(( ($(date +%s) - started_s) / 60 ))m $(( ($(date +%s) - started_s) % 60 ))s"
  echo "  transcript      $LOG"
}


# -------------------------------------------------------------------- report
report() {
  say "stats"
  cd "$HOME"   # not a directory holding a kompressor config, or it wins
  "$KOMPRESSOR" stats summary --config "$CONFIG"
  echo; "$KOMPRESSOR" stats by-provider
  echo; "$KOMPRESSOR" stats by-type
  echo; "$KOMPRESSOR" stats retrievals --config "$CONFIG"

  say "compression and retrieval, whole store"
  python3 "$SWEEP_LIB/runstats.py" "$STATS" 0

  say "cost estimate"
  sqlite3 -header -csv "$STATS" "
    select o.request_id,o.tokens_in,o.tokens_out,p.prompt_tokens,p.cache_read_tokens
    from optimization_events o join proxy_requests p using(request_id)
    order by p.time_ms" > "$WORK/joined.csv"
  python3 - "$WORK/joined.csv" <<'PY'
import csv, sys
rows=list(csv.DictReader(open(sys.argv[1])))
if not rows: sys.exit("no rows — did the sweep reach the proxy?")
g=lambda r,k:(int(r[k]) if r[k] not in ('','None',None) else 0)
# NULL is not zero. A websocket-relayed session has no usage at all: the relay
# never reads a `response.completed` frame, so it cannot know what the provider
# billed and records null rather than a figure it would be inventing. Collapsing
# that to 0 and dividing by the total is how this stage died with a
# ZeroDivisionError on the first sweep that ran over the relay.
reported=lambda k:any(r[k] not in ('','None',None) for r in rows)
tot=sum(g(r,'tokens_in')-g(r,'tokens_out') for r in rows)
last=rows[-1]; uniq=g(last,'tokens_in')-g(last,'tokens_out'); resent=tot-uniq
fresh=sum(g(r,'prompt_tokens')-g(r,'cache_read_tokens') for r in rows)
cached=sum(g(r,'cache_read_tokens') for r in rows)
billed=fresh+cached
have_usage=reported('prompt_tokens') and billed>0
print(f"requests        {len(rows)}")
print(f"removed         {tot}  ({uniq} unique, billed once + {resent} re-sent history)")
if have_usage:
    print(f"billed prompt   {billed}  ({fresh} fresh + {cached} cached = {100*cached/billed:.1f}%)")
else:
    print("billed prompt   not reported — the provider's usage never reached the proxy.")
    print("                A relayed websocket session is the usual reason: the relay")
    print("                carries frames and never reads a `response.completed`, so it")
    print("                records null rather than a number it would be making up.")
print(f"final turn      {g(last,'tokens_out')} instead of {g(last,'tokens_in')}"
      f"  ({g(last,'tokens_in')/max(g(last,'tokens_out'),1):.2f}x context headroom)")
if have_usage:
    F,C=5e-6,5e-7   # $5.00 / $0.50 per 1M input tokens
    w=fresh*F+cached*C; wo=(fresh+uniq)*F+(cached+resent)*C
    if wo>0:
        print(f"\nif billed at API rates:  with ${w:.4f}   without ${wo:.4f}   saving {100*(wo-w)/wo:.1f}%")
    print("NOTE: a ChatGPT plan bills no tokens. The real win is the context headroom above,")
    print("      and this ratio is an upper bound — these fixtures were built to compress.")
else:
    print("\nno API-rate estimate: it needs the provider's own token counts, and this")
    print("run has none. The context headroom above is measured and still stands.")
PY

  # Frames, when there are any. The block above explains why a relayed run
  # reports no usage; this is the file that says what the relay actually did,
  # and it is worth naming in the same breath rather than leaving the reader
  # to guess that a capture exists.
  if [ -s "$FRAMES" ]; then
    say "websocket frames"
    python3 - "$FRAMES" <<'FRAMES_EOF'
import sys
total = carried = 0
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    total += 1
    # A rewritten frame is the whole point of the relay: the engine replaced
    # part of a `response.create` with handles on its way upstream. Counted by
    # the marker rather than by a flag, the same way the cache-loss diff does.
    if "ref:k_" in line:
        carried += 1
print(f"frames          {total}  ({carried} carry handles)")
FRAMES_EOF
    echo "  $FRAMES"
  fi

  say "html report"
  write_report
}

# ------------------------------------------------------------------- analyse
# Per-payload breakdown, replaying what codex actually sent through the engine
# directly. Free, and the only way to see per-payload ratios: the proxy records
# per *request*, and a request carries the whole conversation.
analyse() {
  say "per-payload breakdown"
  local sess; sess=$(newest_session)
  echo "session: $sess"
  rm -rf "$WORK/payloads"; mkdir -p "$WORK/payloads"
  python3 - "$sess" "$WORK/payloads" <<'PY'
import json, sys
src, out = sys.argv[1], sys.argv[2]
n=0
for i,line in enumerate(open(src)):
    p=(json.loads(line).get('payload') or {})
    if p.get('type') not in ('function_call_output','custom_tool_call_output'): continue
    b=p.get('output')
    # Under code mode the result is a *list* of content blocks, and `str()` on a
    # list gives Python's repr — single quotes, not JSON, and not the text the
    # model saw. One run measured that repr instead of the payload and reported
    # 13 of 16 files at 0.00%, which was an artefact of this line.
    if isinstance(b, list):
        b = "".join(x.get("text", "") for x in b if isinstance(x, dict))
    elif isinstance(b, dict):
        b = b.get("content") or json.dumps(b)
    open(f"{out}/p{i:04d}.txt","w").write(str(b)); n+=1
print("tool outputs extracted:", n)
PY
  python3 - "$KOMPRESSOR" "$CONFIG" "$WORK/payloads" <<'PY'
import json, subprocess, glob, os, sys
K, CFG, D = sys.argv[1], sys.argv[2], sys.argv[3]
cmds=['service.log','build.log','small.json','records.json','engine.py','metrics.csv',
      'deploy.yaml','app.toml','server.ini','report.html','ls data/many','grep TODO',
      'git diff','progress.txt','NOTES.md','GUIDE.md']
ti=to=0
print(f"{'#':2} {'command':14} {'detected':12} {'kind':12} {'chars in':>9} {'out':>8} {'saved%':>7}")
for n,f in enumerate(sorted(glob.glob(f"{D}/*.txt"))):
    d=json.loads(subprocess.run([K,'detect','--config',CFG,'--json',f],
                                capture_output=True,text=True).stdout or '{}')
    c=json.loads(subprocess.run([K,'compress','--config',CFG,'--mode','recoverable','--json',f],
                                capture_output=True,text=True).stdout or '{}')
    if not c: continue
    ti+=c['chars_in']; to+=c['chars_out']
    print(f"{n:2} {(cmds[n] if n<len(cmds) else '?'):14} {str(d.get('content_type')):12} "
          f"{str(c.get('kind')):12} {c['chars_in']:9} {c['chars_out']:8} {c['saved_pct']:7.2f}")
print(f"\nunique payload bytes  {ti} -> {to}   {100*(ti-to)/max(ti,1):.2f}% removed")
PY
}

# ----------------------------------------------------------------- retrieval
# Does the CCR loop actually close? Compression is measurable on its own, but
# `recoverable` mode only means anything if the model can get back what was
# removed — and across a dozen runs that has been the weakest link: the tool is
# offered on every compressed request and is almost never called.
#
# So this is a separate, deliberately rigged test rather than part of the sweep.
# It writes a source file whose function bodies are long enough for the
# code-aware compressor to elide them, hides one fact in the middle of one body,
# and asks a question that cannot be answered from anything still visible. The
# agent either calls `optscale_kompressor_retrieve` or gets the answer wrong.
#
# Two things make it fair: the fact is a token generated fresh per run, so it is
# not in the model's weights and not in its earlier context; and the premise is
# checked locally before anything is spent, so a run that proves nothing says so
# instead of billing for it.
retrieval() {
  say "retrieval — can the model fetch back what was elided?"
  local wrap_status=0
  local dir="$WORK/retrieval"
  rm -rf "$dir"; mkdir -p "$dir"
  local token
  token="ZX-$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')-QQ"
  echo "  planted token: $token"
  # Counted as a delta. Run after `sweep` in `all`, the table already holds that
  # run's retrievals, and reporting the total here would credit this test with
  # fetches it did not cause.
  # Wall-clock, not max(id). Two things went wrong with the id: it was compared
  # against a row *count* below — different units subtracted from each other,
  # correct only while the ids happen to run 1..N with no gaps — and a `reset`
  # restarts them, which reads as "nothing was retrieved".
  local before
  before=$(date +%s%3N)

  python3 "$SWEEP_LIB/ledger.py" "$dir" "$token"
  # codex truncates a tool result at roughly 40 KB. The fixture is deliberately
  # larger than the token's offset and smaller than nothing in particular: what
  # matters is that the token survives the cut, which is asserted rather than
  # assumed, because an earlier note here claimed the whole file fit and it does
  # not -- 43,159 bytes against a measured ~40,152-byte cut.
  python3 - "$dir/ledger.py" "$token" <<'SIZE'
import re, sys
body = open(sys.argv[1]).read()
at = body.find(sys.argv[2])
cut = 40152
# The question names a function and a shape; if the fixture stops writing
# either, the run asks something unanswerable and reads the correct answer
# ("no such thing is there") as a wrong one. That is not hypothetical: the
# prompt asked for a string assigned to `audit_checksum`, ledger.py has
# never written that name, and two runs were filed as retrieval failures for
# an answer that was right — measured 2026-09-29.
inside = re.search(r"def reconcile_17\b.*?(?=\ndef |\Z)", body, re.S)
if not inside or sys.argv[2] not in inside.group(0):
    sys.exit("  PREMISE BROKEN: the token is not inside reconcile_17, which is "
             "the function the prompt asks about")
quoted = re.findall(r'compute\(entries, "([^"]*)"', body)
if quoted != [sys.argv[2]]:
    sys.exit(f"  PREMISE BROKEN: the prompt asks for the one quoted argument to "
             f"compute(); the fixture has {len(quoted)}")
# The fixture has to arrive WHOLE, which is a stronger requirement than the
# token surviving a cut, and the old check asked only the weaker one.
#
# codex truncates the middle of an oversized tool result, not the tail: it
# splices the two halves together inside a line and marks the join. The token
# then "survives" — it is still in the payload, so the old check passed — while
# the spliced line stops the parser recovering anything after it, the body
# holding the token is never elided, and the model reads the answer in the
# clear. Three runs were spent on premises that failed this way, and the
# verdicts blamed the retrieve loop. Measured 2026-09-29.
print(f"  fixture: {len(body):,} bytes against codex's ~{cut:,}-byte cap, "
      f"token at {at:,} ({'whole' if len(body) < cut else 'WILL BE TRUNCATED'})")
if len(body) >= cut:
    sys.exit("  PREMISE BROKEN: codex will truncate this payload, splicing its "
             "middle and leaving a line the parser cannot read. Shrink the "
             "fixture in ledger.py.")
SIZE

  # Prove the premise before spending anything: the body must actually be
  # elided, and the token must be gone from what the model will see.
  # Against the FRAMED shape, not the bare file. codex delivers a tool result
  # as `[{"type":"input_text","text":"Script completed…"},{"type":"input_text",
  # "text":<the file>}]`, and that routes differently from the bare bytes —
  # which is why this stage reported INCONCLUSIVE three runs running while the
  # bare-file premise said everything was fine. The engine is asked the same
  # question the proxy will be asked, through the compress API so it costs
  # nothing.
  local compressed
  compressed=$(python3 - "$dir/ledger.py" <<'FRAME' | curl -s -m 60 -X POST "http://127.0.0.1:$PORT/v1/compress" \
      -H 'content-type: application/json' --data-binary @- 
import json, sys
body = open(sys.argv[1]).read()
print(json.dumps({"messages": [{"role": "tool", "content": [
    {"type": "input_text", "text": "Script completed\nWall time 0.1 seconds\nOutput:\n"},
    {"type": "input_text", "text": body}]}]}))
FRAME
)
  if [ -z "$compressed" ]; then
    echo "  the compress API did not answer on 127.0.0.1:$PORT — cannot check the premise"
    return 1
  fi
  # The premise is not "something was elided" but "the answer was, and it is
  # fetchable". Both halves are checked, because either one alone makes the run
  # prove nothing: an output that still shows the token needs no retrieval, and
  # an elision with no handle cannot be retrieved even if the model tries.
  if grep -q "$token" <<<"$compressed"; then
    echo "  the token survived compression — it can be read without retrieving, so"
    echo "  this run would prove nothing. (A uniquely-shaped line is kept in the"
    echo "  clear by design; the fixture hides the token inside a repeated one.)"
    return 1
  fi
  if ! grep -q "ref:k_" <<<"$compressed"; then
    echo "  the output carries no handle — nothing to retrieve, so this would prove nothing"
    return 1
  fi
  # Worth saying what this does and does not establish. It compresses the bare
  # file; the run sends the file through the proxy wrapped in a harness frame
  # inside a JSON message, and that copy can be classified differently and so
  # elided differently. The authoritative check is `planted` after the run,
  # against what the proxy actually stored.
  echo "  premise holds for the framed copy: the token is not in the clear and"
  echo "  the output carries handles, asked of the shape codex actually sends"

  # The file is deleted between reading it and being asked about it. Without
  # that the test proves nothing: an agent with a shell will `grep` for the
  # answer, which never puts the elided body in its context at all — measured,
  # on the first version of this test, which the agent answered correctly after
  # one `grep -C 8` and zero retrievals.
  cat > "$dir/prompt.txt" <<'PROMPT_EOF'
Do these three steps in order.

1. Run: cat ledger.py
2. Run: rm ledger.py
3. Answer this question: inside the function `reconcile_17`, exactly one call
   to `compute(...)` is passed a quoted string where every other call is
   passed a number. What is that exact string? Reply with only that string.

After step 2 the file is gone, so do not try to read it again and do not
recreate it. Everything you need arrived in step 1. If part of what you received
was replaced by a marker, use the tool that fetches the original back before you
answer. Do not guess and do not answer from memory.
PROMPT_EOF

  cd "$dir"
  git init -q . >/dev/null 2>&1 || true
  PATH="$HOME/.local/bin:$PATH" timeout 900 \
    "$KOMPRESSOR" wrap codex --no-proxy --config "$CONFIG" -- \
      exec --skip-git-repo-check --sandbox workspace-write "${MODEL_ARGS[@]}" \
      -c 'plugins."codex-app-tools@openai-bundled".enabled=false' \
      "$(cat "$dir/prompt.txt")" < /dev/null > "$dir/run.log" 2>&1 || wrap_status=$?
  # Same two outcomes as the sweep, told apart by `agent_failed`.
  if [ "${wrap_status:-0}" -ne 0 ]; then
    agent_failed retrieval "$dir/run.log" "$wrap_status"
    return 1
  fi

  say "retrieval result"
  cd "$HOME"
  "$KOMPRESSOR" stats retrievals --config "$CONFIG"
  local after fetched returned planted
  after=$(sqlite3 "$STATS" "select count(*) from retrieval_events" 2>/dev/null || echo 0)
  fetched=$(sqlite3 "$STATS" "select count(*) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  # A retrieval that returned nothing is not evidence. One run fetched a
  # fragment with a range that matched no line, got 0 characters back, and still
  # answered correctly — so counting fetches alone reports PASS for an answer
  # that came from somewhere else entirely.
  returned=$(sqlite3 "$STATS" "select coalesce(sum(returned_chars),0) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  # And the premise has to hold against what the proxy *stored*, not against a
  # local dry run: the file reaches the engine wrapped in a harness frame inside
  # a JSON message, which can route it differently from the bare bytes.
  planted=$(sqlite3 "$REFS" "select count(*) from refs where cast(fragment as text) like '%$token%'" 2>/dev/null || echo 0)
  # What the proxy did in flight, which is the only evidence bearing on this
  # run. `planted` above cannot tell the two apart: the premise check
  # compresses through the same live store, so the fragment it finds may be the
  # one it minted itself a second earlier. Three runs were certified that way
  # while the live request left the answer in the clear — 13 handles in flight
  # against the premise check's 26, measured 2026-09-29.
  local live_refs premise_refs
  live_refs=$(sqlite3 "$STATS" "select coalesce(max(refs_emitted),0) from \
    optimization_events where surface='proxy' and time_ms >= $before" \
    2>/dev/null || echo 0)
  premise_refs=$(sqlite3 "$STATS" "select coalesce(max(refs_emitted),0) from \
    optimization_events where surface='api' and time_ms >= $before" \
    2>/dev/null || echo 0)
  # The token is in the transcript no matter what: it is a line of the file
  # the model was told to `cat`. Only the final message decides this.
  local answered="no" final
  final="$(final_answer "$dir/run.log")"
  case "$final" in *"$token"*) answered="yes" ;; esac
  local reread="no"
  [ -f "$dir/ledger.py" ] && reread="yes"
  echo
  echo "  retrievals recorded : $fetched   (this stage only; $after in the table)"
  echo "  characters returned : $returned"
  echo "  token was elided    : $([ "${planted:-0}" -gt 0 ] && echo yes || echo no)   (fragments holding it: $planted)"
  echo "  elided in flight    : $live_refs handles   (the premise check got $premise_refs)"
  echo "  answered correctly  : $answered   (token $token)"
  if session_died "$dir/run.log"; then
    local why; why=$(session_death "$dir/run.log")
    echo "  INCONCLUSIVE — the codex session failed before it read anything:"
    echo "                 ${why:-an upstream error}. Nothing was shown to the model,"
    echo "                 so nothing could be fetched. Check codex's own login."
    verdict retrieval INCONCLUSIVE "reason=agent session failed upstream" \
            "detail=${why:-upstream error}"
  elif [ "${planted:-0}" -eq 0 ]; then
    echo "  INCONCLUSIVE — no stored fragment holds the token, so it was never"
    echo "                 elided and the model did not need to retrieve it."
    echo "                 The run proves nothing whatever the answer was."
    echo
    echo "                 The likely cause is routing, not the engine: the premise"
    echo "                 above is checked by compressing the bare file, while the"
    echo "                 run sends it through the proxy inside a harness frame in a"
    echo "                 JSON message. That can classify differently — source_code"
    echo "                 for the file, text for the framed copy — and a different"
    echo "                 route elides a different region, leaving the token in the"
    echo "                 clear. Compare 'detected' in the per-payload breakdown"
    echo "                 against the bare-file type to confirm."
    verdict retrieval INCONCLUSIVE "reason=token never elided"
  elif [ "$answered" = "yes" ] && [ "${fetched:-0}" -gt 0 ] && [ "${returned:-0}" -gt 0 ]; then
    echo "  PASS — the token was elided into a fragment, the model fetched it back,"
    echo "         and the answer came out of what it fetched"
    verdict retrieval PASS "fetched=$fetched" "returned=$returned" "answered=yes"
  elif [ "$answered" = "yes" ] && [ "${fetched:-0}" -gt 0 ]; then
    echo "  INCONCLUSIVE — a retrieval happened but returned $returned characters, so"
    echo "                 the answer did not come from it"
    verdict retrieval INCONCLUSIVE "fetched=$fetched" "returned=$returned"
  elif [ "$answered" = "yes" ] && [ "${fetched:-0}" -eq 0 ] \
       && [ "${live_refs:-0}" -lt "${premise_refs:-0}" ]; then
    echo "  INCONCLUSIVE — the premise held for the premise check and not in flight."
    echo "                 The proxy emitted $live_refs handles for this payload where"
    echo "                 compressing it directly got $premise_refs, so the body holding"
    echo "                 the token went upstream in the clear and there was"
    echo "                 nothing to fetch. Whatever the model answered, this"
    echo "                 says nothing about the retrieve loop: fix the elision"
    echo "                 first, then re-run."
    verdict retrieval INCONCLUSIVE "reason=premise failed in flight" \
            "live_refs=$live_refs" "premise_refs=$premise_refs"
  elif [ "$answered" = "yes" ] && [ "$reread" = "yes" ]; then
    echo "  INCONCLUSIVE — right answer, no retrieval, and ledger.py is still on disk:"
    echo "                 the agent read it again rather than fetching the fragment"
    verdict retrieval INCONCLUSIVE "reason=file re-read instead of fetched"
  elif [ "$answered" = "yes" ]; then
    echo "  INCONCLUSIVE — right answer with no retrieval recorded, and the file is"
    echo "                 gone. Check run.log: it either kept the body in context"
    echo "                 (nothing was elided) or answered from something else"
    verdict retrieval INCONCLUSIVE "reason=right answer, no retrieval recorded"
  else
    echo "  FAIL — the elided body was never fetched and the answer is wrong."
    echo "         That is the CCR loop not closing, which is what this test exists to show."
    verdict retrieval FAIL "fetched=$fetched" "answered=no"
  fi
}

# Start the proxy detached, with optional environment overrides.
#
# Two things here are not decoration. `9>&-` closes the lock fd in the child:
# `hold_the_lock` opens it with `exec 9>`, bash does not set close-on-exec, and
# a long-lived child inherits it — the proxy then holds the flock for as long as
# it runs, so every later stage refuses to start with "another sweep stage is
# running" and no sweep in sight. `setsid --fork` puts it in its own session, so
# the script is not left in `do_wait` on a server that never exits, holding the
# same lock from the other end. Both were live bugs, minutes apart.
# Appends rather than truncates, and that matters more than it looks. Several
# stages restart the proxy (`restart_proxy`, `bughunt_proxy`), and a truncating
# redirect meant the last restart erased everything the sweep had logged — so a
# run finished with a serve.log holding one "listening" line and no record of
# what the proxy did. Met while trying to tell a relayed websocket session from
# an HTTPS one: the single line that says which it was had been overwritten by a
# later stage. `reset` deletes the file, so a fresh run still starts clean.
start_proxy() {
  # `--config` and `--listen` are not optional decoration. Without them `serve`
  # runs its own configuration discovery — $KOMPRESSOR_CONFIG, a kompressor.toml
  # in the cwd, then the per-user default — and binds whatever `listen` it finds
  # there. Every other command in this script passes `--config "$CONFIG"` and
  # talks to `127.0.0.1:$PORT`, so the proxy was the one process free to
  # disagree with the rest of the run.
  #
  # On a machine where $CONFIG happens to be the discovery default and $PORT is
  # 8817 the two agree by coincidence and nothing looks wrong. Override either
  # and they diverge: with CONFIG= set the proxy served a different config than
  # the stages measured against, and with PORT= set it ignored the port and
  # failed to bind. Both measured here.
  #
  # The relay override goes first so an explicit `"$@"` from a caller still
  # wins: `bughunt_proxy` sets its own environment and must keep the last word.
  #
  # The frame capture is named on every run, not only a relayed one. The proxy
  # writes the file only when it actually relays something, so naming it costs
  # nothing when the relay is off and means a run that turns it on needs no
  # second decision. The Harbor bench has done this since 2026-10-09; this
  # script is the one that actually exercises the relay, and was the one not
  # capturing it.
  local relay=(KOMPRESSOR_WS_CAPTURE="$FRAMES"
    KOMPRESSOR__PROXY__WEBSOCKET_RELAY="$(relay_wanted)")
  # Record it before launching, so `ensure_proxy` on a later run can tell what
  # the listener it finds was started with. Nothing else can: the settings are
  # environment, and the only other way to read a running process's
  # environment is /proc/<pid>/environ, which also holds the API key.
  {
    # The capture path and the resolved relay, then any caller overrides. The
    # relay line is the effective setting rather than the array above, so this
    # agrees with `proxy_env_wanted`; see `proxy_effective_config`.
    local effective
    effective="$(proxy_effective_config)"
    echo "KOMPRESSOR_WS_CAPTURE=$FRAMES"
    echo "relay=$(relay_of "$effective")"
    if [ "$#" -gt 0 ]; then printf '%s\n' "$@"; fi
    echo "effective-sha256=$(digest_of "$effective")"
  } > "$PROXY_ENV"
  echo "  websocket frames -> $FRAMES  (written only when the relay is on)"
  setsid --fork env "${relay[@]}" "$@" "$KOMPRESSOR" serve --proxy --api \
    --config "$CONFIG" --listen "127.0.0.1:$PORT" \
    >> "$WORK/serve.log" 2>&1 < /dev/null 9>&-
}

# What a run is about to do, before it does it. `all` takes twenty minutes and
# spends quota; saying which stages cost money, and in what order, is cheaper
# than finding out from the bill.
plan() {
  printf '\n\033[1m== plan: %s\033[0m\n' "$*"
  local s
  for s in "$@"; do
    case "$s" in
      setup)     echo "  setup      build+install kompressor, check codex and login   (free)" ;;
      reset)     echo "  reset      DELETE stats.db and refs.db, restart the proxy    (free, destructive)" ;;
      fixtures)  echo "  fixtures   build 16 payloads and measure each one            (free)" ;;
      sweep)     echo "  sweep      one codex session reading all 16                  (PAID)" ;;
      report)    echo "  report     stats, cost estimate, kompressor-report.html      (free)" ;;
      analyse)   echo "  analyse    replay what codex sent, per payload               (free)" ;;
      retrieval) echo "  retrieval  can the model fetch back a planted token?         (PAID)" ;;
      bughunt)   echo "  bughunt    code compressor + retrieval, eight languages      (PAID)" ;;
    esac
  done
}

# Where everything landed, so the run ends with somewhere to look.
finish() {
  local began=$1

  # The paid tests run after `report`, so the report it wrote does not know how
  # they went. Regenerate at the end, with both verdicts and both tables in it —
  # one page that describes the whole run rather than its first two thirds.
  say "paid test results"
  python3 "$SWEEP_LIB/verdict.py" "$VERDICTS" --text

  say "final report"
  write_report

  say "done in $(( ($(date +%s) - began) / 60 ))m $(( ($(date +%s) - began) % 60 ))s"
  echo "  html report     $HOME/kompressor-report.html"
  echo "  sweep log       $LOG"
  echo "  bug hunt log    $WORK/bughunt-run.log"
  echo "  fixture sizes   $WORK/fixture-languages.json"
  echo "  language sizes  $WORK/bughunt-languages.json"
  echo "  stats           $STATS"
  echo
  echo "  re-read the numbers any time without spending:"
  echo "    $0 report"
}

# Record a paid test's outcome so the final report can carry it. Written to a
# file rather than kept in a variable: the stages are separate shell functions
# and a run can stop between them, which is exactly when you want to know which
# tests had got as far as answering.
verdict() {
  # Every verdict carries the model and the client that produced it, appended
  # here rather than at each call site so none can be recorded without them.
  python3 "$SWEEP_LIB/verdict.py" "$VERDICTS" "$@" \
    "model=$(codex_model)" "codex=$(codex_version)" || true
}

# Build ~/kompressor-report.html: the generator's own page, plus whatever this
# run has measured.
#
# One function and three callers, because three callers used to write three
# different pages: `report` wrote the plain generator output, which silently
# overwrote the enriched page a finished `all` had just produced. The sections
# come from files on disk, so re-running `report` tomorrow still reproduces the
# full page for free — a missing file simply drops its section.
write_report() {
  local html="$HOME/kompressor-report.html"
  "$KOMPRESSOR" stats report --config "$CONFIG" > "$html" 2>/dev/null \
    || "$KOMPRESSOR" stats report > "$html" 2>/dev/null \
    || { echo "  could not write $html"; return 1; }
  local pairs=""
  [ -s "$VERDICTS" ] && pairs="$SWEEP_LIB/verdict.py $VERDICTS"
  [ -s "$WORK/bughunt-languages.json" ] && \
    pairs="$pairs | $SWEEP_LIB/bughunt-table.py $WORK/bughunt-languages.json --preset=bughunt"
  [ -s "$WORK/fixture-languages.json" ] && \
    pairs="$pairs | $SWEEP_LIB/bughunt-table.py $WORK/fixture-languages.json --preset=fixtures"
  # shellcheck disable=SC2086
  [ -n "$pairs" ] && python3 "$SWEEP_LIB/bughunt-embed.py" "$html" --pairs ${pairs#| } >/dev/null
  ls -la "$html"
}

# One sweep at a time. `reset` deletes the stats and refs stores and restarts
# the proxy, so a second run started while one is in flight does not merely add
# noise — it removes the rows the first one is about to read, and both sessions
# then compete for the same proxy and the same plan quota. That happened twice
# in one morning, and both times the damage looked like a bug in the thing
# being measured. The lock is held for the whole stage and released on exit.
hold_the_lock() {
  mkdir -p "$WORK"
  exec 9>"$WORK/.sweep.lock"
  if ! flock -n 9; then
    echo "another sweep stage is running (lock: $WORK/.sweep.lock)."
    echo "Held by:"
    fuser -v "$WORK/.sweep.lock" 2>&1 | sed 's/^/  /'
    echo "If nothing above looks like a sweep, a previous run leaked the fd to a"
    echo "child; kill that process and try again."
    exit 1
  fi
}

# Did the agent session actually run, or did it die before doing any work?
#
# A transport or credential failure leaves a transcript indistinguishable, by
# counts alone, from a model that ignored every instruction: no reads, no
# retrievals, no answer. Judged on counts the paid stages call that FAIL, and
# they have — measured 2026-09-26, where bughunt reported "the bodies were
# elided, nothing was fetched, and the bug was missed" for a session that never
# opened a file. The cause was codex presenting a service-account key the
# provider rejected, reproduced with codex unwrapped and no proxy in the path,
# so the verdict named this proxy for someone else's fault.
#
# 405 is excluded deliberately: codex tries a websocket first, the proxy does
# not speak one, and codex falls back to HTTPS and carries on. That is noise on
# every healthy run, not death.
session_died() {
  local log="$1"
  [ -f "$log" ] || return 1
  grep -qE 'unexpected status (40[0-46-9]|4[1-9][0-9]|5[0-9][0-9])' "$log" 2>/dev/null
}

# The upstream status and message, for a verdict that names what went wrong.
#
# Same exclusion as `session_died`, and it has to be: the websocket 405 is the
# first "unexpected status" in every transcript, so a plain first-match here
# reported "405 Method Not Allowed" for a session that actually died on a 401 —
# pointing the reader at the one error that was never the problem.
session_death() {
  grep -oE 'unexpected status (40[0-46-9]|4[1-9][0-9]|5[0-9][0-9]) [A-Za-z ]+' "$1" \
    2>/dev/null | head -1
}

# Wait for the proxy to answer on $PORT. `start_proxy` detaches, so every
# caller has to wait for the listener rather than assume it.
wait_for_proxy() {
  local i
  for i in $(seq 1 20); do
    curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && return 0
    sleep 1
  done
  return 1
}

# Restart the proxy, optionally with environment overrides, and wait for it.
# The stats and refs stores are left alone — this is not `reset`.
bughunt_proxy() {
  local pid; pid=$(ss -lptnH "sport = :$PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)
  [ -n "$pid" ] && { kill "$pid"; sleep 2; }
  start_proxy "$@"
  wait_for_proxy || {
    echo "  proxy did not come back on $PORT; see $WORK/serve.log"
    return 1
  }
}

# ------------------------------------------------------------------ bughunt
# The code compressor and the retrieve loop, tested together, across every
# language the engine knows.
#
# Eight source files, twelve near-identical functions each, and one
# out-of-bounds read. Every body is long enough to be elided, so what the model
# receives is twelve signatures and twelve identical markers.
#
# Two things stop this degenerating into a reading test. Compression is
# deterministic, so re-reading a file gives the same elided view — there is no
# second path to the body. And the faulty line is byte-identical to its eleven
# siblings: all twelve end `return rows[bound]`, and `bound` is assigned in the
# middle of the elided region. No known-bad shape exists anywhere in the tree,
# so a scanner looking for `rows[len(rows)]` finds nothing.
#
# It is still not search-proof: a model that reads all eight files and compares
# the twelve `bound =` lines can win without retrieving. The prompt forbids
# content search, and the verdict below checks whether that was honoured before
# believing a correct answer.
bughunt() {
  say "bug hunt — the code compressor and the retrieve loop, in eight languages"
  local wrap_status=0
  local dir="$WORK/bughunt" log="$WORK/bughunt-run.log" prompt="$WORK/bughunt-prompt.txt"
  local langs="$WORK/bughunt-languages.json" html="$HOME/kompressor-report.html"
  # The workdir holds fixtures and nothing else. An earlier version kept the
  # transcript in here and the model read it, which handed over every line the
  # compressor had just elided.
  rm -rf "$dir"; mkdir -p "$dir"; rm -f "$log"
  python3 "$SWEEP_LIB/bughunt.py" "$dir" >/dev/null

  # The shipped budget is three rounds, and a model that fetches one body per
  # round sees three of twelve before being refused — measured twice, both
  # ending in "the retrieval budget was exhausted before I could inspect them
  # all". True of the default, and useless as an instrument, so the stage runs
  # against a raised budget and says so. Telling the model to batch its calls
  # into a single turn was tried first; it did not.
  local rounds="${BUGHUNT_ROUNDS:-16}"
  echo "  retrieve_max_rounds for this stage: $rounds (shipped default is 3)"
  bughunt_proxy "KOMPRESSOR__PROXY__RETRIEVE_MAX_ROUNDS=$rounds" || return 1

  # A watermark in wall-clock rather than in max(id): a restart that starts the
  # table's ids over reads as "nothing was retrieved" and turns every count
  # below into a silent zero. That happened once already.
  local before
  before=$(date +%s%3N)

  # Premise, per language, before anything is spent: the assignment that
  # distinguishes the faulty function has to be gone, and the output has to
  # carry a handle. A language that fails either test is dropped and named —
  # an answer about seven languages beats no answer about eight.
  #
  # The same compression feeds the efficiency table below, deliberately: run
  # separately, the table could describe a different compression from the one
  # the premise was checked against, and a table that does not describe what
  # the model was actually given is worse than no table.
  local ok
  ok=$(python3 "$SWEEP_LIB/bughunt-measure.py" \
         "$KOMPRESSOR" "$CONFIG" "$dir" "$langs")
  echo "  $ok language(s) ready"
  if [ "$ok" -lt 2 ]; then
    echo "  too few languages elide the bug; this would prove nothing"
    # The reason per language is already in the JSON and used to be thrown
    # away, so a machine where *every* language failed printed a bare "0
    # language(s) ready" and nothing else — the same output for a config that
    # cannot be read, a fixture generator that changed, and an engine that
    # stopped eliding. Measured on a second machine, where the cause was a
    # missing config file and the message named none of it.
    python3 - "$langs" <<'PY'
import json, sys
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
bad = [r for r in rows if not r.get("ok")]
if bad:
    print("  why each one was dropped:")
    for r in bad:
        print(f"    {r.get('file',''):14} {r.get('why') or 'unknown'}")
    if all((r.get("why") or "") == "would not compress" for r in bad):
        print()
        print("  every language failed to compress at all, which is usually the")
        print("  configuration rather than the engine. Check that the file exists")
        print("  and is readable:")
PY
    # Outside the heredoc so the shell expands it.
    if [ ! -f "$CONFIG" ]; then
      echo "    $CONFIG  <- MISSING"
      echo "  The proxy starts without it (it falls back to built-in defaults),"
      echo "  so setup passes and only the --config calls fail. Create one, or"
      echo "  point CONFIG= at an existing file."
    fi
    return 1
  fi
  echo
  python3 "$SWEEP_LIB/bughunt-table.py" "$langs"

  cat > "$prompt" <<'PROMPT_EOF'
Each file in this directory defines twelve near-identical functions. Exactly
one function reads one element past the end of its array. Every function ends
with the same line, so the fault is only visible part-way inside a body.

Read the files and name the faulty function. Answer with just the function
name, once, on its own line.

Three rules, and the result is void if any is broken:

1. Read whole files only — `cat file`, or your file-reading tool. Do not search
   inside files. No grep, rg, sed, awk, perl, or scripts in python/node that
   open files and print matching lines. This measures whether elided bodies can
   be recovered, and a search sidesteps the thing being measured.

2. Read exactly ONE file per turn. Do not combine reads into a single script,
   cell or loop, and do not cat several files at once. One `cat ledger.c`, then
   stop and look at what came back, then the next file. Reading them together
   produces one large mixed-language blob instead of one file, which is not
   what this measures.

3. If a body has been replaced by a marker saying it was omitted, fetch the
   original back with the retrieval tool before answering. Do not guess from
   the names — all twelve are named alike on purpose — and do not answer from
   memory.

   Retrieval is budgeted by *turn*, not by call, and the budget is small. Ask
   for every body you need in one turn — several retrieval calls side by side —
   rather than one per turn. Fetching them one at a time runs the budget out
   before you have seen enough to answer. This is the one place several calls
   belong together; rule 2 still applies to reading files.
PROMPT_EOF

  cd "$dir"
  PATH="$HOME/.local/bin:$PATH" timeout 1200 9>&- \
    "$KOMPRESSOR" wrap codex --no-proxy --config "$CONFIG" -- \
      exec --skip-git-repo-check --sandbox workspace-write "${MODEL_ARGS[@]}" \
      -c 'plugins."codex-app-tools@openai-bundled".enabled=false' \
      "$(cat "$prompt")" < /dev/null > "$log" 2>&1 || wrap_status=$?
  # Same two outcomes as the sweep, told apart by `agent_failed`.
  if [ "${wrap_status:-0}" -ne 0 ]; then
    agent_failed bughunt "$log" "$wrap_status"
    return 1
  fi

  say "bug hunt result"
  cd "$HOME"
  "$KOMPRESSOR" stats by-type
  echo
  "$KOMPRESSOR" stats retrievals --config "$CONFIG"
  local fetched returned answered searched onbug refs_db
  # Everything below is scoped to the rows this stage added. Summing the whole
  # table reports last week's retrievals as if they were this run's.
  fetched=$(sqlite3 "$STATS" "select count(*) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  returned=$(sqlite3 "$STATS" "select coalesce(sum(returned_chars),0) from retrieval_events where time_ms >= $before" 2>/dev/null || echo 0)
  # Did a fragment that actually holds the answer come back? `retrievals > 0`
  # only says the loop turned; this says it turned on the right body. The
  # handle carries a `k_` prefix in the stats table and not in the store.
  refs_db="$(dirname "$STATS")/refs.db"
  onbug=$(sqlite3 "$STATS" \
    "attach '$refs_db' as r;
     select count(*) from retrieval_events e join r.refs f on f.ref = substr(e.ref, 3)
     where e.time_ms >= $before and e.found = 1
       and (cast(f.fragment as text) like '%bound = size%'
            or cast(f.fragment as text) like '%bound := size%');" \
    2>/dev/null || echo 0)
  # The function name appears in twelve signatures in every file, so searching
  # the whole transcript for it matches the fixtures rather than the answer.
  # Only what codex printed after its last token count is the answer.
  answered="no"
  if grep -qiE '^[^a-z0-9_]*reconcile07[^a-z0-9_]*$' \
       <<<"$(final_answer "$log")"; then
    answered="yes"
  fi
  # Rule 1, checked rather than trusted: a search that reaches the raw file
  # returns a few short lines, and short output is not worth compressing, so
  # the answer arrives uncompressed and the retrieval proves nothing.
  searched=$(tr -d '\r' < "$log" | grep -cE "bash -lc .*(grep|rg |egrep|sed -n .*/p|awk |perl -[ne]|read_text|readFileSync|open\()" || true)

  # Rule 2, checked the same way and for the same reason. Under code mode the
  # agent can batch every read into one `exec` cell, and eight languages then
  # arrive as one mixed blob classified as a single payload. The code
  # compressor has nothing to work with: measured 2.94% on a three-file blob
  # against 89.29% on the same file alone. Nothing is elided, no marker is
  # shown, and the model simply reads the bodies and answers correctly.
  #
  # Counted rather than inferred, because on the counts alone that run is
  # indistinguishable from a model that ignored the markers: right answer, zero
  # retrievals, zero searches. It was reported as INCONCLUSIVE on retrieval
  # grounds, which sent the reader looking at the retrieve loop for a premise
  # failure — measured 2026-09-26.
  #
  # Counted from the command the model ran, not from what came back. The first
  # version of this looked for a `FILE x` header per file in the tool result,
  # which nothing in this harness emits, so it scored 0 on every run and
  # printed that 0 beside a PASS as though it were compliance — measured
  # 2026-09-28. The result is also the wrong place to look: `ls` output names
  # all eight files without reading any of them.
  batched=$(python3 - "$(newest_session)" "$dir" <<'BATCH_EOF' || echo 0
import fnmatch, json, os, re, sys

try:
    path, fixtures = sys.argv[1], sys.argv[2]
except IndexError:
    print(0); raise SystemExit
try:
    names = [f for f in os.listdir(fixtures) if not f.startswith(".")]
except OSError:
    names = []
if not path or not names:
    print(0); raise SystemExit

# `ledger.c` is a prefix of `ledger.cpp`, so a plain substring count reports
# two files where the model read one.
exact = [(n, re.compile(r"(?<![\w.-])%s(?![\w])" % re.escape(n))) for n in names]


def files_touched(cmd):
    hit = {n for n, rx in exact if rx.search(cmd)}
    # `cat ledger.*` names no file and reads seven.
    for token in re.findall(r"[\w./*?\[\]-]*[*?][\w./*?\[\]-]*", cmd):
        hit |= {n for n in names if fnmatch.fnmatch(n, os.path.basename(token))}
    return len(hit)


worst = 0
try:
    for line in open(path, errors="replace"):
        try:
            payload = json.loads(line).get("payload") or {}
        except ValueError:
            continue
        # The call, never the result: a result carries the one file it read,
        # and an `ls` result carries the names of all eight.
        if not str(payload.get("type", "")).endswith("call"):
            continue
        cmd = (payload.get("input") or payload.get("arguments")
               or payload.get("action") or "")
        if not isinstance(cmd, str):
            cmd = json.dumps(cmd)
        worst = max(worst, files_touched(cmd))
except OSError:
    pass
# 0 means no read was found at all, which is a broken detector or a dead
# session rather than a compliant run. The caller prints it as such.
print(worst)
BATCH_EOF
)

  echo
  echo "  languages tested    : $ok"
  echo "  retrievals recorded : $fetched   (this stage only)"
  echo "  characters returned : $returned"
  echo "  named reconcile07   : $answered   (final answer only, not the transcript)"
  echo "  fetched the bug body: $onbug   (fragments containing the faulty line)"
  echo "  content searches    : $searched   (rule 1 says zero)"
  if [ "${batched:-0}" -gt 0 ]; then
    echo "  files per read      : $batched   (rule 2 says one)"
  else
    echo "  files per read      : unknown   (no file read found in the session log)"
  fi
  echo "  transcript          : $log"
  # Only now: restarting the proxy earlier would race the stats writer for the
  # rows this verdict is built from.
  bughunt_proxy || true   # back to the shipped budget for every other stage

  if session_died "$log"; then
    local why; why=$(session_death "$log")
    echo "  INCONCLUSIVE — the codex session failed before it read anything:"
    echo "                 ${why:-an upstream error}."
    echo "                 No file was opened, so no marker was ever shown and there"
    echo "                 was nothing to fetch. This says nothing about retrieval."
    echo "                 Check codex's own login first — if it reproduces with"
    echo "                 \`codex exec\` unwrapped, the proxy is not involved."
    verdict bughunt INCONCLUSIVE "reason=agent session failed upstream" \
            "detail=${why:-upstream error}" "languages=$ok"
  elif [ "${searched:-0}" -gt 0 ]; then
    echo "  VOID — the model searched inside the files, so it never needed the markers."
    echo "         Whatever it answered, this run says nothing about retrieval."
    verdict bughunt VOID "reason=model searched inside the files" "languages=$ok"
  elif [ "${batched:-0}" -gt 1 ]; then
    # Before any verdict that reasons about retrieval: with nothing elided
    # there was nothing to retrieve, so counting retrievals here measures the
    # batching and not the loop.
    echo "  VOID — the model read $batched files in one go, so the engine saw one"
    echo "         mixed-language blob rather than one file and elided nothing."
    echo "         With no marker shown there was nothing to fetch, whatever it"
    echo "         answered. Rule 2 exists to stop this; re-run, and if it"
    echo "         persists the prompt is not holding the model to one read."
    verdict bughunt VOID "reason=batched reads defeated the premise" \
            "files_per_read=$batched" "languages=$ok"
  elif [ "$answered" = "yes" ] && [ "${onbug:-0}" -gt 0 ]; then
    echo "  PASS — the body was elided, the model fetched back the one holding the"
    echo "         faulty line, and it named the function"
    verdict bughunt PASS "languages=$ok" "fetched=$fetched" "bug_body_fetched=$onbug"
  elif [ "$answered" = "yes" ]; then
    verdict bughunt INCONCLUSIVE "languages=$ok" "fetched=$fetched" "bug_body_fetched=0"
    echo "  INCONCLUSIVE — right answer, but no fragment carrying the faulty line came"
    echo "                 back. Twelve identical markers give a 1-in-12 guess, so a"
    echo "                 correct answer is not on its own evidence. Read the transcript."
  elif grep -q 'retrieval budget' "$log"; then
    verdict bughunt FAIL "reason=retrieval budget exhausted" "languages=$ok" "fetched=$fetched"
    echo "  FAIL — the model ran out of retrieval budget before it had seen enough."
    echo "         It fetched $fetched bodies, one per turn, of the twelve in a file."
    echo "         Raise BUGHUNT_ROUNDS, or read this as what the budget costs a"
    echo "         model that will not batch its calls."
  elif [ "${fetched:-0}" -gt 0 ]; then
    echo "  FAIL — it fetched bodies back and still did not find the bug"
    verdict bughunt FAIL "reason=fetched but did not find it" "fetched=$fetched"
  else
    echo "  FAIL — the bodies were elided, nothing was fetched, and the bug was missed."
    echo "         That is the CCR loop not closing on the task it exists for."
    verdict bughunt FAIL "reason=nothing fetched, bug missed" "languages=$ok"
  fi

  say "compression by language"
  python3 "$SWEEP_LIB/bughunt-table.py" "$langs"
  python3 "$SWEEP_LIB/bughunt-table.py" "$langs" --by-type

  say "html report"
  write_report
}

# The comment header, whatever length it has grown to. This used to be
# `sed -n '2,12p'`, a fixed range that silently stopped printing whatever was
# added past line 12 — which is how a documented option becomes invisible.
usage_text() { sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d'; }

# One stage per invocation. The dispatch below reads `$1` and nothing else, so
# `... setup fixtures` used to run `setup` and discard `fixtures` without a
# word — the caller waits for a stage that never ran and reads the absence as
# a failure of the stage itself.
if [ "$#" -gt 1 ]; then
  echo "one stage at a time: got $# ($*)"
  echo "run them in sequence, or use \`all\`."
  exit 1
fi

BEGAN=$(date +%s)
# No default stage on purpose. `all` deletes stats.db and refs.db and spends
# quota, so a bare invocation — or a stray Enter — must not start it.
case "${1:-usage}" in
  setup)    setup ;;
  reset)    hold_the_lock; reset ;;
  fixtures) fixtures ;;
  # `setup` first, like `retrieval` and `bughunt`: `sweep` runs
  # `wrap codex --no-proxy`, which by design refuses to start a proxy and fails
  # with "nothing is serving" on a machine that has none. Every other paid stage
  # already went through `setup`, so this one was alone in requiring the user to
  # have started a proxy by hand.
  sweep)    hold_the_lock; plan setup sweep; setup; sweep ;;
  report)   report ;;
  analyse)  analyse ;;
  retrieval) hold_the_lock; plan setup retrieval; setup; retrieval ;;
  bughunt)  hold_the_lock; plan setup bughunt; setup; bughunt ;;
  all)      hold_the_lock
            rm -f "$VERDICTS"   # a stale PASS from yesterday is worse than a blank
            plan setup reset fixtures sweep report analyse retrieval bughunt
            setup; reset; fixtures; sweep; report; analyse; retrieval; bughunt
            finish "$BEGAN" ;;
  usage)    usage_text; exit 0 ;;
  *)        echo "unknown stage: $1"; usage_text; exit 1 ;;
esac
