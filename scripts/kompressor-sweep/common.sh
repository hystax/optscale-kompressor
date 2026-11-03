#!/usr/bin/env bash
# Shared by `codex.sh` and `claude.sh`.
#
# What lives here is what is genuinely the same question for both agents:
# where the helper scripts are, and whether a paid run proved anything. The
# per-agent halves stay in their own files, because they are not the same
# problem wearing different names — codex rewrites `~/.codex/config.toml` and
# routes subscription traffic to `chatgpt.com/backend-api/codex`, while Claude
# Code is redirected by an `env` block in `~/.claude/settings.json` and needs
# `frozen_prefix = "first_seen"` before it compresses at all.
#
# Deliberately NOT shared: proxy start/stop. codex.sh reuses a running proxy
# when its configuration digest still matches, and that logic is load-bearing
# there; claude.sh starts a dedicated one per stage. Forcing one shape on both
# would be a rewrite of the older script, not a deduplication.

# The payload builders and scorers, which never knew which agent drives them.
SWEEP_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"

# ---------------------------------------------------------------------------
# did this run prove anything?
# ---------------------------------------------------------------------------
#
# Three ways a paid stage can be meaningless, each of which has been mistaken
# for an engine result at least once:
#
#   1. the session never reached the proxy        -> nothing was compressed
#   2. the proxy elided nothing in front of it    -> no marker, nothing to fetch
#   3. the model searched inside the files        -> it never needed the marker
#
# A verdict that does not rule these out is reporting the harness, not the
# engine. `$1` is a wall-clock milliseconds stamp taken before the session.

# Requests that reached the proxy during this stage.
sweep_requests_since() {
  sqlite3 "$1" "select count(*) from proxy_requests where time_ms >= $2;" 2>/dev/null || echo 0
}

# Handles the LIVE session was shown.
#
# Not the same question as "can this fixture be elided": the premise checks
# compress through the `api` surface, which answers only the second. A run
# whose conversation emitted no handle had no marker in front of the model,
# so its answer says nothing about retrieval however it came out.
sweep_live_handles() {
  sqlite3 "$1" "select coalesce(sum(refs_emitted),0) from optimization_events \
    where surface='proxy' and time_ms >= $2;" 2>/dev/null || echo 0
}

# Retrievals, and how much they actually returned.
#
# A fetch that returned nothing is not evidence: one run fetched a range that
# matched no line, got 0 characters back, and still answered — so counting
# fetches alone reports a pass for an answer that came from somewhere else.
sweep_retrievals_since() {
  sqlite3 "$1" "select count(*) from retrieval_events where time_ms >= $2;" 2>/dev/null || echo 0
}
sweep_returned_since() {
  sqlite3 "$1" "select coalesce(sum(returned_chars),0) from retrieval_events \
    where time_ms >= $2;" 2>/dev/null || echo 0
}

# Did a fragment holding the bughunt fault come back? `retrievals > 0` says the
# loop turned; this says it turned on the right body.
sweep_bug_body_fetched() {
  local stats="$1" refs="$2" since="$3"
  sqlite3 "$stats" \
    "attach '$refs' as r;
     select count(*) from retrieval_events e join r.refs f on f.ref = substr(e.ref, 3)
     where e.time_ms >= $since and e.found = 1
       and (cast(f.fragment as text) like '%bound = size%'
            or cast(f.fragment as text) like '%bound := size%');" 2>/dev/null || echo 0
}

# Rule 1 of the bughunt prompt, checked rather than trusted. A search reaches
# the raw file and returns a few short lines; short output is not worth
# compressing, so the answer arrives uncompressed and proves nothing.
sweep_searched() {
  tr -d '\r' < "$1" 2>/dev/null \
    | grep -cE '(grep|rg |egrep|sed -n .*/p|awk |perl -[ne]|readFileSync)' || true
}
