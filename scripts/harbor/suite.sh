#!/usr/bin/env bash
# terminal-bench through the proxy, at three sizes.
#
#   export UPSTREAM=https://api.openai.com
#   export OPENAI_API_KEY=sk-...
#   scripts/harbor/suite.sh smoke     # 1 task, ~$2, ~10 min
#   scripts/harbor/suite.sh suite     # 10 tasks
#   scripts/harbor/suite.sh full      # every task in the dataset (~89)
#   TASKS='mailman regex-chess' TAG=pair scripts/harbor/suite.sh tasks
#
# MODE=recoverable (the default) is the mode that has something to prove:
# elisions keep a handle and the proxy executes `optscale_kompressor_retrieve` itself.
# MODE=aggressive drops retention; MODE=off is a passthrough baseline over the
# same network path, which is the only honest comparison -- a baseline taken
# without the proxy also removes a hop.
#
# SKIP_HARBOR=1 runs every gate and stops before spending anything.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STAGE="${1:-smoke}"
# Where the harbor checkout lives. Searched rather than assumed: the previous
# default hardcoded one person's directory layout, so the script worked on
# exactly one machine and failed with a confusing "no such file" everywhere
# else. Every candidate below is relative to this checkout, so the search
# behaves the same from any clone of this repository. Set HARBOR to skip it.
HARBOR="${HARBOR:-}"
if [ -z "$HARBOR" ]; then
  for candidate in \
    "$ROOT/../harbor" \
    "$ROOT/../kiroframe_internal/snippets/harbor" \
    "$ROOT/../../opensource/kiroframe_internal/snippets/harbor" \
    "$ROOT/../../session/harbor"; do
    if [ -d "$candidate" ]; then HARBOR="$(cd "$candidate" && pwd)"; break; fi
  done
fi
if [ ! -x "$HARBOR/.venv/bin/harbor" ]; then
  echo "no harbor checkout found${HARBOR:+ at $HARBOR}." >&2
  echo "set HARBOR=/path/to/harbor (the directory holding .venv/bin/harbor)" >&2
  exit 2
fi
KOMPRESSOR_BIN="${KOMPRESSOR_BIN:-$ROOT/target/release/kompressor}"
MODEL="${MODEL:-gpt-5.5}"
# Which provider the agent talks to. This picks three things that have to agree
# — the harbor agent, the model namespace, and the environment variables that
# agent reads — plus the upstream mount and preflight inside harbor/bench.py.
# Setting one without the others is a run that looks configured and is not.
PROVIDER="${PROVIDER:-openai}"
MODE="${MODE:-recoverable}"
PORT="${KOMPRESSOR_PORT:-8817}"
RUNS="${KOMPRESSOR_RUNS:-$ROOT/runs}"
# -n is CONCURRENT TRIALS, -k is ATTEMPTS PER TASK. They are not the same flag.
# One trial has been measured at ~368K tokens/min, so concurrency 3 wants about
# 1.1M TPM of provider headroom; drop it if your tier is lower.
CONCURRENCY="${CONCURRENCY:-3}"
ATTEMPTS="${ATTEMPTS:-1}"
LIMIT="${LIMIT:-}"
EXCLUDE="${EXCLUDE:-}"
# NO_RETRIEVE=1 stops the proxy offering optscale_kompressor_retrieve. Not only a
# safety-net question: the tool definition is in the schema on every request.
NO_RETRIEVE="${NO_RETRIEVE:-}"
TAG="${TAG:-}"

: "${UPSTREAM:?set UPSTREAM to the provider base, e.g. https://api.openai.com}"
case "$PROVIDER" in
  openai)
    : "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
    API_KEY="$OPENAI_API_KEY"
    AGENT="codex"
    MODEL_SPEC="openai/{model}"
    # codex reads both spellings depending on the path it takes.
    AGENT_ENV=(--ae "OPENAI_API_KEY={vk}" --ae "OPENAI_BASE_URL={base_url}"
               --ae "OPENAI_API_BASE={base_url}")
    ;;
  anthropic)
    : "${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY}"
    API_KEY="$ANTHROPIC_API_KEY"
    AGENT="claude-code"
    MODEL_SPEC="anthropic/{model}"
    # Claude Code appends `/v1/messages` itself, so it is given the root and
    # not the `/v1` an OpenAI client is handed. ENABLE_TOOL_SEARCH is not
    # optional: with a custom base URL and this unset, Claude Code stops
    # deferring tool schemas and inlines every one of them, which makes the
    # prompt larger — the opposite of the thing being measured (GH #746).
    AGENT_ENV=(--ae "ANTHROPIC_API_KEY={vk}" --ae "ANTHROPIC_BASE_URL={base_url_root}"
               --ae "ENABLE_TOOL_SEARCH=true" --ae "ANTHROPIC_MODEL=$MODEL")
    # `ANTHROPIC_MODEL` is not redundant with `-m anthropic/$MODEL`. Harbor's
    # claude-code agent decides the model name like this:
    #
    #     elif "ANTHROPIC_BASE_URL" in env:
    #         # custom base URL (OpenRouter, self-hosted), keep full model name
    #         env["ANTHROPIC_MODEL"] = self.model_name
    #
    # — i.e. a custom base URL is taken to mean a gateway that wants the
    # namespaced `anthropic/claude-sonnet-4-5`. This proxy is not that gateway:
    # it forwards to the real API, which answers 404 for that model name. The
    # first run of this cost a trial to exactly that, with the proxy relaying
    # Anthropic's 404 faithfully while its own preflight — which used the bare
    # name — returned 200. Setting it explicitly takes the branch out of play.
    ;;
  *) echo "PROVIDER must be openai or anthropic, got '$PROVIDER'" >&2; exit 2 ;;
esac
[[ -x "$KOMPRESSOR_BIN" ]] || { echo "no release binary at $KOMPRESSOR_BIN; run 'make release'" >&2; exit 1; }

# A fixed list, so two runs of `suite` are comparable and so are runs of this
# against another compressor. Changing it invalidates every earlier comparison.
SUITE_TASKS=(
  circuit-fibsqrt cobol-modernization llm-inference-batching-scheduler
  mailman make-doom-for-mips mteb-leaderboard path-tracing
  portfolio-optimization pytorch-model-recovery regex-chess
)

# The provider is part of the identity of a run, so it is part of the path —
# without it an anthropic smoke and an openai smoke land in the same directory
# and the second silently becomes the record of the first.
#
# Only when it is *not* openai, though. Every run on disk predates this option
# and is openai; suffixing those too would rename the paths out from under a
# history that exists to be compared against.
PROVIDER_SUFFIX=""
[ "$PROVIDER" = "openai" ] || PROVIDER_SUFFIX="_$PROVIDER"
SUFFIX="${MODE}${PROVIDER_SUFFIX}${NO_RETRIEVE:+_noretrieve}${TAG:+_$TAG}"
case "$STAGE" in
  smoke) TASKS=(mailman);             OUT="$RUNS/harbor_smoke_$SUFFIX" ;;
  suite) TASKS=("${SUITE_TASKS[@]}"); OUT="$RUNS/harbor_suite_$SUFFIX" ;;
  full)  TASKS=();                    OUT="$RUNS/harbor_full_$SUFFIX" ;;
  # An arbitrary list, for investigating a particular set of tasks. It gets its
  # own output directory keyed by TAG, because a hand-picked list is not
  # comparable to `suite` and must not be filed next to it.
  tasks)
    [ -n "${TASKS:-}" ] || { echo "stage 'tasks' needs TASKS='a b c'" >&2; exit 2; }
    read -r -a TASKS <<< "$TASKS"
    OUT="$RUNS/harbor_tasks_$SUFFIX" ;;
  *) echo "usage: $0 {smoke|suite|full|tasks}" >&2; exit 2 ;;
esac

# No -i at all means every task in the dataset; that is how `full` selects ~89.
INCLUDES=""
for t in ${TASKS[@]+"${TASKS[@]}"}; do INCLUDES+=" -i terminal-bench/$t"; done
for t in $EXCLUDE; do INCLUDES+=" -x terminal-bench/$t"; done
[ -n "$LIMIT" ] && INCLUDES+=" -l $LIMIT"

# The timeout multiplier is what lets a reasoning model finish the hard build
# tasks. Without it they die mid-debug and the accuracy delta is noise.
export HARBOR_CMD="$HARBOR/.venv/bin/harbor run \
  -m $MODEL_SPEC -d terminal-bench/terminal-bench-2-1 \
 $INCLUDES \
  -a $AGENT -o {out_dir} -n $CONCURRENCY -k $ATTEMPTS -y \
  --agent-timeout-multiplier 3.0 \
  --allow-agent-host {advertise} \
  ${AGENT_ENV[*]}"

# Optional flags are built at top level and echoed. bash expands an unset array
# to nothing without a word of complaint, so a flag assigned inside a branch
# that was not taken vanishes silently -- which is how a long paid run once came
# back measuring a configuration nobody asked for.
RETRIEVE_ARG=()
if [ -n "$NO_RETRIEVE" ]; then
  RETRIEVE_ARG=(--no-retrieve-tool)
  echo "retrieve arg -> ${RETRIEVE_ARG[*]}"
fi
EXTRA_ARG=()
if [ -n "${SKIP_HARBOR:-}" ]; then
  EXTRA_ARG=(--skip-harbor)
  echo "extra arg -> ${EXTRA_ARG[*]}"
fi

mkdir -p "$OUT"
N_TASKS=${#TASKS[@]}
[ "$N_TASKS" -eq 0 ] && N_TASKS="ALL${LIMIT:+ (capped at $LIMIT)}"
echo "stage=$STAGE  tasks=$N_TASKS  attempts=$ATTEMPTS  concurrency=$CONCURRENCY"
echo "provider=$PROVIDER  agent=$AGENT  model=$MODEL_SPEC"
echo "mode=$MODE${NO_RETRIEVE:+  retrieve_tool=OFF}${TAG:+  tag=$TAG}  out=$OUT"
# `KOMPRESSOR__PROXY__FROZEN_PREFIX` included because a run under it looks
# exactly like a run without it -- near-zero compression -- and the only
# way to tell them apart afterwards was to grep the effective config.
for v in KOMPRESSOR_MIN_TOKENS KOMPRESSOR_MIN_CHARS KOMPRESSOR_MAX_REF_BYTES \
         KOMPRESSOR__PROXY__FROZEN_PREFIX KOMPRESSOR__PROXY__RETRIEVE_TOOL \
         KOMPRESSOR__PROXY__CACHE_TTL KOMPRESSOR__ENGINE__ENABLE_CODE_AWARE; do
  [ -n "${!v:-}" ] && echo "  env $v=${!v}"
done
# Provider-scoped settings (`proxy.providers`), whatever their names.
for v in $(compgen -e | grep '^KOMPRESSOR__PROXY__PROVIDERS__' || true); do
  echo "  env $v=${!v}"
done
echo "watch it with: KOMPRESSOR_RUNS=$RUNS $ROOT/scripts/harbor/watch.py"

exec /usr/bin/env python3 "$ROOT/scripts/harbor/bench.py" \
  --upstream "$UPSTREAM" --api-key "$API_KEY" --model "$MODEL" \
  --provider "$PROVIDER" \
  --kompressor-bin "$KOMPRESSOR_BIN" --mode "$MODE" \
  --port "$PORT" --out-dir "$OUT" \
  "${RETRIEVE_ARG[@]}" "${EXTRA_ARG[@]}"
