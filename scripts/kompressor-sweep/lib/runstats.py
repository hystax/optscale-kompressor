"""Summarise one stage's compression, from the rows that stage wrote.

    python3 runstats.py <stats.db> <since_ms> [label] [--proxy-only]

Scoped by wall-clock rather than by row id: a reset restarts the ids and every
figure would silently read as zero.
Use --proxy-only to exclude local compression-premise checks from live totals.
"""
import sqlite3
import sys
from collections import Counter
import json

db, since = sys.argv[1], int(sys.argv[2])
surface_filter = " and surface='proxy'" if "--proxy-only" in sys.argv else ""
conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
row = conn.execute(f"""
    select count(*), coalesce(sum(tokens_in),0), coalesce(sum(tokens_out),0),
           coalesce(sum(chars_in),0), coalesce(sum(chars_out),0),
           coalesce(sum(messages_compressed),0), coalesce(sum(messages_total),0),
           coalesce(sum(refs_emitted),0), coalesce(sum(ref_bytes_emitted),0),
           coalesce(sum(engine_ms),0.0)
    from optimization_events where time_ms >= ?{surface_filter}""", (since,)).fetchone()
(reqs, ti, to, ci, co, mc, mt, refs, refb, ms) = row

if not reqs:
    print("  no compression events — did the agent reach the proxy?")
    sys.exit(0)

types = Counter()
for (blob,) in conn.execute(
        f"select content_types from optimization_events where time_ms >= ?{surface_filter}", (since,)):
    try:
        for k, v in json.loads(blob).items():
            types[k] += v
    except ValueError:
        pass

# A retrieval the budget refused before the store was read. It is an ask the
# model made and did not get, and it belongs in neither side of a hit rate:
# counting it as an attempt divides hits by asks the store never saw, counting
# it as a miss blames the store for a budget the operator set. So every figure
# below excludes it and it is printed on its own line.
#
# This run reported `hits 7 (100.0%), 0 missed` for a relayed connection that
# had been refused 11 times, which is what the column was added for.
#
# Probed rather than assumed: the column arrived in schema v10, and a db
# written by an older binary has the rows without it.
has_withheld = any(r[1] == "withheld" for r in
                   conn.execute("pragma table_info(retrieval_events)"))
served = "withheld = 0" if has_withheld else "1"
ret = conn.execute(f"""
    select coalesce(sum(case when {served} then 1 else 0 end),0),
           coalesce(sum(case when {served} then found else 0 end),0),
           coalesce(sum(case when {served} then returned_tokens else 0 end),0),
           coalesce(sum(case when {served} then expired else 0 end),0),
           coalesce(sum(case when {served} then ranged else 0 end),0),
           count(distinct case when {served} then ref end),
           coalesce(sum(case when {served} then returned_chars else 0 end),0),
           {'coalesce(sum(withheld),0)' if has_withheld else '0'}
    from retrieval_events where time_ms >= ?""", (since,)).fetchone()
asks, hits, back_tokens, expired, ranged, distinct, back_chars, withheld = ret
top = conn.execute(f"""
    select ref, sum(case when {served} then 1 else 0 end), sum(case when {served} then found else 0 end),
           sum(case when {served} then returned_tokens else 0 end),
           {'coalesce(sum(withheld),0)' if has_withheld else '0'}
    from retrieval_events where time_ms >= ?
    group by ref order by count(*) desc, sum(returned_tokens) desc limit 5""",
    (since,)).fetchall()

pct = lambda a, b: (100.0 * (a - b) / a) if a else 0.0
mib = lambda n: f"{n / 1048576:.1f} MiB"

print(f"  requests        {reqs:,}")
print(f"  messages        {mc:,} compressed of {mt:,} seen")
print(f"  tokens          {ti:,} -> {to:,}   ({pct(ti, to):.2f}% removed)")
print(f"  characters      {ci:,} -> {co:,}   ({pct(ci, co):.2f}% removed)")
print(f"  fragments kept  {refs:,} handles, {mib(refb)}")
print(f"  engine time     {ms / 1000:.2f}s total, {ms / reqs:.1f}ms per request")
if types:
    print(f"  message types   {' · '.join(f'{k} {v}' for k, v in types.most_common())}")

print()
print("  retrieval")
if not asks and not withheld:
    # The number this project keeps coming back to. Minting a handle costs
    # storage and a marker in the prompt; a handle nobody fetches is both of
    # those spent for nothing, so "0 of N" is a result, not a blank.
    print(f"    nothing fetched back — 0 of {refs:,} handles minted were ever asked for")
else:
    print(f"    asks          {asks} over {distinct} distinct handles")
    if asks:
        print(f"    hits          {hits} ({100.0 * hits / asks:.1f}%), "
              f"{asks - hits} missed of which {expired} expired")
        print(f"    ranged        {ranged} ({100.0 * ranged / asks:.1f}%) — a slice, not the whole fragment")
    if withheld:
        print(f"    withheld      {withheld} refused before the store was read — the per-request")
        print(f"                  token budget was already spent. Not counted above: these are")
        print(f"                  asks the model made and did not get. Raise")
        print(f"                  proxy.retrieve_max_tokens_per_request to let them through.")
    print(f"    returned      {back_tokens:,} tokens, {back_chars:,} chars")
    print(f"    of the store  {distinct} of {refs:,} handles minted here were fetched "
          f"({100.0 * distinct / refs:.2f}%)" if refs else "")
    print(f"    net tokens    {ti - to - back_tokens:,} removed after retrieval put content back")
    if top:
        print("    most asked")
        for ref, n, h, tok, held in top:
            line = (f"      {ref}  {n} ask{'s' if n != 1 else ''}, "
                    f"{h} hit{'s' if h != 1 else ''}, {tok:,} tokens")
            if held:
                line += f", {held} withheld"
            print(line)

