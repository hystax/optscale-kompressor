-- Manual queries for a kompressor stats store (schema v5).
-- Run it from the directory holding the databases, so no path here is true on
-- only one machine:
--
--   cd ~/.local/share/kompressor        # or /var/lib/kompressor, or a run dump
--   sqlite3 -header -column stats.db < <repo>/scripts/db/queries.sql
-- Per-run stores live beside each dump; scripts/db/merge.py combines them; scripts/db/refs.sql reads the fragment store.

.print === sessions in this store ===
SELECT api_family, model, COUNT(*) req,
       datetime(MIN(time_ms)/1000,'unixepoch','localtime') first,
       datetime(MAX(time_ms)/1000,'unixepoch','localtime') last
FROM proxy_requests GROUP BY 1,2 ORDER BY 3 DESC;

.print
.print === compression, discarded rewrites excluded (they saved nothing) ===
SELECT model, COUNT(*) n, SUM(tokens_in) tin, SUM(tokens_out) tout,
       ROUND(100.0*(SUM(tokens_in)-SUM(tokens_out))/NULLIF(SUM(tokens_in),0),1) removed_pct,
       SUM(refs_emitted) handles, ROUND(AVG(engine_ms),1) engine_ms
FROM optimization_events WHERE outcome='compressed' GROUP BY 1;

.print
.print === why the engine declined things (the V5 skip attribution) ===
SELECT * FROM v_skip_reasons ORDER BY tokens DESC;

.print
.print === per conversation: cache behaviour. EXCESS WRITES = write - final prefix.
.print === Anthropic only -- OpenAI reports cache_write as NULL, so it reads as 0.
SELECT conversation, turns, cache_write, cache_read, completion FROM v_conversations;

.print
.print === excess writes per conversation (run against an ANTHROPIC store) ===
WITH last AS (
  SELECT conversation,
         COALESCE(cache_read_tokens,0)+COALESCE(cache_write_tokens,0) AS final_prefix,
         ROW_NUMBER() OVER (PARTITION BY conversation ORDER BY time_ms DESC) rn
  FROM proxy_requests WHERE conversation IS NOT NULL)
SELECT p.conversation, COUNT(*) turns,
       SUM(COALESCE(p.cache_write_tokens,0)) written, l.final_prefix,
       SUM(COALESCE(p.cache_write_tokens,0)) - l.final_prefix AS excess_writes
FROM proxy_requests p JOIN last l
  ON l.conversation=p.conversation AND l.rn=1
GROUP BY 1;

.print
.print === the daily economy that survives retention ===
-- Raw rows are pruned at the retention horizon; these archives are what is left
-- after that, so a window older than the horizon has to be read from here.
-- Per-turn ordering does not survive, so excess writes cannot be computed from
-- this -- only the daily totals.
SELECT day, tenant, requests, prompt_tokens, cache_read_tokens,
       cache_write_tokens, completion_tokens, conversations
FROM v_daily_requests ORDER BY day DESC LIMIT 14;

.print
.print === retrieval: how often the model actually asks for a fragment back ===
SELECT (SELECT SUM(refs_emitted) FROM optimization_events) handles_minted,
       COUNT(*) retrievals,
       SUM(found) hits, SUM(ranged) narrowed, SUM(returned_tokens) put_back,
       ROUND(1.0*(SELECT SUM(refs_emitted) FROM optimization_events)/NULLIF(COUNT(*),0),0)
         AS handles_per_retrieval
FROM retrieval_events;

.print
.print === what kinds of content the engine found ===
SELECT * FROM v_type_coverage;

.print
.print === request economy and outcomes ===
SELECT * FROM v_outcomes;

.print
.print === Tier 0 gate measurements (schema v18; also /v1/stats/{restarts,retrieve-rounds,fetch-probability}) ===
-- The same SQL the endpoints run (kompressor-stats measured.rs; a unit test
-- holds the two together). Parameters: the busiest tenant, all of recorded
-- history, every experiment. Change them here to narrow the window, e.g.
--   .parameter set :from_ms 1795000000000
--   .parameter set :experiment_id "'t0-baseline'"
.parameter init
.parameter set :tenant "(SELECT tenant FROM proxy_requests GROUP BY tenant ORDER BY COUNT(*) DESC LIMIT 1)"
.parameter set :from_ms 0
.parameter set :to_ms 9223372036854775807
.parameter set :experiment_id NULL

.print
.print === restarts against open conversations: per process start, conversations
.print === with a client turn in the hour before / after / both, what the last turn
.print === before had cached (read + write) and what the first turn after read
WITH starts AS (
  SELECT id, time_ms, version, surfaces FROM process_starts
   WHERE time_ms >= :from_ms AND time_ms < :to_ms),
turns AS (
  SELECT p.id, p.conversation, p.time_ms,
         COALESCE(p.cache_read_tokens, 0) AS cache_read,
         COALESCE(p.cache_write_tokens, 0) AS cache_write
    FROM proxy_requests p
   WHERE p.tenant = :tenant AND p.keep_warm = 0 AND p.conversation IS NOT NULL
     AND p.time_ms >= :from_ms - 3600000 AND p.time_ms < :to_ms + 3600000
     AND (:experiment_id IS NULL OR EXISTS (
          SELECT 1 FROM request_context c
           WHERE c.tenant = p.tenant AND c.request_id = p.request_id
             AND json_extract(c.context, '$.experiment_id') = :experiment_id))),
before AS (
  SELECT s.id AS start_id, t.conversation, t.time_ms, t.cache_read + t.cache_write AS cached,
         ROW_NUMBER() OVER (PARTITION BY s.id, t.conversation
                            ORDER BY t.time_ms DESC, t.id DESC) AS nth
    FROM starts s JOIN turns t
      ON t.time_ms >= s.time_ms - 3600000 AND t.time_ms < s.time_ms),
after AS (
  SELECT s.id AS start_id, t.conversation, t.time_ms, t.cache_read AS read,
         ROW_NUMBER() OVER (PARTITION BY s.id, t.conversation
                            ORDER BY t.time_ms, t.id) AS nth
    FROM starts s JOIN turns t
      ON t.time_ms >= s.time_ms AND t.time_ms < s.time_ms + 3600000),
across AS (
  SELECT b.start_id, b.cached, a.read, a.time_ms - b.time_ms AS gap_ms
    FROM before b JOIN after a
      ON a.start_id = b.start_id AND a.conversation = b.conversation
   WHERE b.nth = 1 AND a.nth = 1)
SELECT s.id, s.time_ms, s.version, s.surfaces,
  (SELECT COUNT(*) FROM before b WHERE b.start_id = s.id AND b.nth = 1) AS conversations_before,
  (SELECT COUNT(*) FROM after a WHERE a.start_id = s.id AND a.nth = 1) AS conversations_after,
  (SELECT COUNT(*) FROM across x WHERE x.start_id = s.id) AS conversations_across,
  (SELECT COALESCE(SUM(x.cached), 0) FROM across x WHERE x.start_id = s.id) AS cached_before,
  (SELECT COALESCE(SUM(x.read), 0) FROM across x WHERE x.start_id = s.id) AS read_after,
  (SELECT COALESCE(SUM(MAX(x.cached - x.read, 0)), 0) FROM across x
    WHERE x.start_id = s.id) AS lost_tokens,
  (SELECT COUNT(*) FROM across x WHERE x.start_id = s.id AND 2 * x.read < x.cached) AS cold_after,
  (SELECT COUNT(*) FROM across x
    WHERE x.start_id = s.id AND x.gap_ms <= 300000) AS across_within_ttl,
  (SELECT COALESCE(SUM(x.cached), 0) FROM across x
    WHERE x.start_id = s.id AND x.gap_ms <= 300000) AS cached_before_within_ttl,
  (SELECT COALESCE(SUM(x.read), 0) FROM across x
    WHERE x.start_id = s.id AND x.gap_ms <= 300000) AS read_after_within_ttl
FROM starts s
ORDER BY s.time_ms, s.id;

.print
.print === retrieve-round cost, measured per round (round 0 is the client's call)
.print === assumed_extra_cache_read is the old estimate: summed read split evenly
WITH requests AS (
  SELECT p.tenant, p.request_id, p.api_family, p.model, p.retrieve_rounds,
         COALESCE(p.cache_read_tokens, 0) AS cache_read
    FROM proxy_requests p
   WHERE p.tenant = :tenant AND p.keep_warm = 0
     AND p.time_ms >= :from_ms AND p.time_ms < :to_ms
     AND (:experiment_id IS NULL OR EXISTS (
          SELECT 1 FROM request_context c
           WHERE c.tenant = p.tenant AND c.request_id = p.request_id
             AND json_extract(c.context, '$.experiment_id') = :experiment_id))),
rounds AS (
  SELECT r.tenant, r.request_id,
         SUM(r.round >= 1) AS extra,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.input_tokens, 0) ELSE 0 END) AS input,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.cache_read_tokens, 0) ELSE 0 END) AS cache_read,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.cache_write_tokens, 0) ELSE 0 END) AS cache_write,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.output_tokens, 0) ELSE 0 END) AS output,
         SUM(CASE WHEN r.round = 0 THEN COALESCE(r.cache_read_tokens, 0) ELSE 0 END) AS first_read
    FROM retrieve_round_usage r
   WHERE r.tenant = :tenant AND r.time_ms >= :from_ms AND r.time_ms < :to_ms
   GROUP BY r.tenant, r.request_id)
SELECT q.api_family, COALESCE(q.model, '') AS model,
       COUNT(*) AS client_turns,
       SUM(q.retrieve_rounds > 0) AS requests_with_rounds,
       SUM(q.retrieve_rounds) AS extra_rounds_recorded,
       SUM(CASE WHEN q.retrieve_rounds > 0
                THEN q.cache_read * q.retrieve_rounds / (q.retrieve_rounds + 1.0)
                ELSE 0 END) AS assumed_extra_cache_read,
       SUM(r.extra > 0) AS requests_measured,
       COALESCE(SUM(r.extra), 0) AS extra_rounds_measured,
       COALESCE(SUM(r.input), 0) AS extra_input,
       COALESCE(SUM(r.cache_read), 0) AS extra_cache_read,
       COALESCE(SUM(r.cache_write), 0) AS extra_cache_write,
       COALESCE(SUM(r.output), 0) AS extra_output,
       COALESCE(SUM(CASE WHEN r.extra > 0 THEN r.first_read END), 0) AS first_round_cache_read
  FROM requests q
  LEFT JOIN rounds r ON r.tenant = q.tenant AND r.request_id = q.request_id
 GROUP BY q.api_family, COALESCE(q.model, '')
 ORDER BY q.api_family, model;

.print
.print === fetch probability by kind, fragment size and prompt size
WITH shipped AS (
  SELECT e.tenant, e.ref, e.request_id, e.kind, e.bytes
    FROM elisions e
   WHERE e.tenant = :tenant AND e.time_ms >= :from_ms AND e.time_ms < :to_ms
     AND (:experiment_id IS NULL OR EXISTS (
          SELECT 1 FROM request_context c
           WHERE c.tenant = e.tenant AND c.request_id = e.request_id
             AND json_extract(c.context, '$.experiment_id') = :experiment_id))),
carried AS (
  SELECT s.kind, s.bytes,
    (SELECT CASE WHEN p.api_family = 'anthropic_messages'
                 THEN COALESCE(u.input_tokens, p.prompt_tokens)
                      + COALESCE(u.cache_read_tokens, p.cache_read_tokens, 0)
                      + COALESCE(u.cache_write_tokens, p.cache_write_tokens, 0)
                 ELSE COALESCE(u.input_tokens, p.prompt_tokens) END
       FROM proxy_requests p
       LEFT JOIN retrieve_round_usage u
         ON u.tenant = p.tenant AND u.request_id = p.request_id AND u.round = 0
      WHERE p.tenant = s.tenant AND p.request_id = s.request_id AND p.keep_warm = 0
      ORDER BY p.id LIMIT 1) AS prefix_tokens,
    EXISTS (SELECT 1 FROM retrieval_events r
             WHERE r.tenant = s.tenant AND r.ref IN (s.ref, substr(s.ref, 3))
               AND r.withheld = 0) AS fetched
    FROM shipped s)
SELECT kind,
  CASE WHEN bytes < 1024 THEN '<1KiB'
       WHEN bytes < 4096 THEN '1-4KiB'
       WHEN bytes < 16384 THEN '4-16KiB'
       WHEN bytes < 65536 THEN '16-64KiB'
       ELSE '>=64KiB' END AS size_bucket,
  CASE WHEN prefix_tokens IS NULL THEN 'unknown'
       WHEN prefix_tokens < 10000 THEN '<10k'
       WHEN prefix_tokens < 50000 THEN '10-50k'
       WHEN prefix_tokens < 100000 THEN '50-100k'
       WHEN prefix_tokens < 200000 THEN '100-200k'
       ELSE '>=200k' END AS prefix_bucket,
  COUNT(*) AS elisions,
  SUM(fetched) AS fetched
FROM carried
GROUP BY 1, 2, 3
ORDER BY 1, 2, 3;
