//! `kompressor stats report`: one self-contained HTML page.
//!
//! The other reports answer one question each and are meant to be read in a
//! terminal. This one is for the case where compression has been left running
//! for days behind an agent and somebody wants to know whether it is worth
//! keeping — a question that needs the headline, the trend, the per-type split
//! and the retrieval side on the same page.
//!
//! # Self-contained on purpose
//!
//! No JavaScript, no web fonts, no CDN, no image files: one file that renders
//! from `file://` on a machine with no network, and keeps rendering in a year.
//! The charts are inline SVG built here. A report that needs to fetch a chart
//! library is a report that eventually shows a blank page, and the whole point
//! of writing it to disk is that it can be kept and compared.
//!
//! # It shows net next to gross, always
//!
//! For the reason [`crate::stats`] gives: gross is the flattering number and
//! this workspace knows it over-counts an agent loop, because re-sent history
//! is credited on every turn it is sent. A page that showed only gross would
//! be a nicer-looking version of a number we know to be wrong.

use kompressor_stats::{
    Bucket, BucketRow, ProviderCoverage, RetrievalSummary, StatsReader, Summary, TypeCoverage,
};

/// Everything the page draws, gathered before any of it is rendered.
pub struct ReportData {
    pub summary: Summary,
    pub buckets: Vec<BucketRow>,
    pub types: Vec<TypeCoverage>,
    pub providers: Vec<ProviderCoverage>,
    pub retrievals: RetrievalSummary,
    pub bucket: Bucket,
    pub generated_ms: i64,
}

impl ReportData {
    /// Read every section for one window.
    pub fn gather(
        reader: &StatsReader,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        bucket: Bucket,
        generated_ms: i64,
    ) -> Result<ReportData, kompressor_stats::StatsError> {
        Ok(ReportData {
            summary: reader.summary(tenant, from_ms, to_ms)?,
            buckets: reader.histogram(tenant, from_ms, to_ms, bucket)?,
            types: reader.by_type(tenant, from_ms, to_ms)?,
            providers: reader.by_provider(tenant, from_ms, to_ms)?,
            retrievals: reader.retrievals(tenant, from_ms, to_ms)?,
            bucket,
            generated_ms,
        })
    }
}

/// The version of the binary rendering the page.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Render the page.
pub fn render(data: &ReportData) -> String {
    let s = &data.summary;
    let kept = if s.tokens_in > 0 {
        100.0 * (s.saved_net as f64) / (s.tokens_in as f64)
    } else {
        0.0
    };
    let window = format!(
        "{} → {}",
        stamp(s.from_ms.max(0)),
        if s.to_ms == i64::MAX {
            "now".to_string()
        } else {
            stamp(s.to_ms)
        }
    );
    let tenant = if s.tenant.is_empty() {
        "(default)".to_string()
    } else {
        escape(&s.tenant)
    };

    let mut out = String::with_capacity(16 * 1024);
    out.push_str(&format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"generator\" content=\"kompressor {VERSION}\">\n\
         <title>kompressor — compression report</title>\n<style>{CSS}</style>\n</head>\n<body>\n"
    ));
    // The build that drew the page, not the one that recorded the numbers: a
    // database outlives upgrades, so a report can describe traffic an older
    // version compressed. Said here so a page passed around can be traced to
    // the code that rendered it.
    out.push_str(&format!(
        "<header>\n<h1>kompressor</h1>\n\
         <p class=\"sub\">tenant <b>{tenant}</b> · window {window} · generated {} \
         by kompressor {VERSION}</p>\n</header>\n",
        stamp(data.generated_ms)
    ));

    out.push_str("<section class=\"cards\">\n");
    // Read left to right these are one sentence: this much arrived, this much
    // was saved once retrieval had been paid for, that is what retrieval cost,
    // and that is the flattering number it was subtracted from.
    out.push_str(&card("tokens in", &num(s.tokens_in), "before compression"));
    out.push_str(&card(
        "tokens net",
        &num(s.saved_net),
        &format!(
            "{} gross \u{2212} {} retrieved",
            num(s.saved_gross),
            num(s.retrieved_tokens)
        ),
    ));
    out.push_str(&card(
        "retrieved",
        &num(s.retrieved_tokens),
        "put back by hits",
    ));
    out.push_str(&card(
        "gross",
        &num(s.saved_gross),
        "removed by compression",
    ));
    out.push_str(&card(
        "of the prompt",
        &format!("{kept:.1}%"),
        "net, over tokens in",
    ));
    out.push_str(&card("requests", &num(s.requests as i64), "compressions"));
    out.push_str(&card(
        "messages",
        &format!("{} / {}", num(s.messages_compressed), num(s.messages_total)),
        "rewritten / seen",
    ));
    out.push_str(&card(
        "retrieval",
        &format!("{:.0}%", s.hit_rate * 100.0),
        &format!(
            "{} asks, {} hits",
            num(s.retrievals as i64),
            num(s.retrieval_hits as i64)
        ),
    ));
    out.push_str("</section>\n");

    out.push_str(&format!(
        "<section>\n<h2>Savings over time</h2>\n\
         <p class=\"note\">Net per {}. A bar below the line is a window where retrieval \
         put back more than compression removed.</p>\n{}</section>\n",
        match data.bucket {
            Bucket::Hour => "hour",
            Bucket::Day => "day",
        },
        chart(&data.buckets)
    ));

    out.push_str("<section>\n<h2>What is being compressed</h2>\n");
    if data.types.is_empty() {
        out.push_str("<p class=\"note\">Nothing recorded in this window.</p>\n");
    } else {
        let widest = data
            .types
            .iter()
            .map(|t| t.messages)
            .max()
            .unwrap_or(1)
            .max(1);
        out.push_str(
            "<table>\n<tr><th>type</th><th class=\"n\">messages</th>\
                      <th class=\"n\">share</th><th class=\"bar\"></th></tr>\n",
        );
        for t in &data.types {
            let w = 100.0 * (t.messages as f64) / (widest as f64);
            out.push_str(&format!(
                "<tr><td><code>{}</code></td><td class=\"n\">{}</td>\
                 <td class=\"n\">{:.1}%</td>\
                 <td class=\"bar\"><span style=\"width:{w:.1}%\"></span></td></tr>\n",
                escape(&t.content_type),
                num(t.messages),
                t.share * 100.0
            ));
        }
        out.push_str("</table>\n");
        out.push_str(
            "<p class=\"note\">Counts, not ratios: the engine records which type claimed a \
             message, and a type that appears often is where a change would be felt. \
             <code>listing</code>, <code>search</code> and <code>log</code> compress heavily; \
             <code>text</code> and <code>source_code</code> barely at all.</p>\n",
        );
    }
    out.push_str("</section>\n");

    out.push_str("<section>\n<h2>Where it went</h2>\n");
    if data.providers.is_empty() {
        out.push_str("<p class=\"note\">Nothing recorded in this window.</p>\n");
    } else {
        out.push_str(
            "<table>\n<tr><th>provider</th><th class=\"n\">requests</th>\
                      <th class=\"n\">tokens in</th><th class=\"n\">removed</th>\
                      <th class=\"n\">share</th><th>sent to</th></tr>\n",
        );
        for p in &data.providers {
            let name = if p.provider.is_empty() {
                "(unnamed)".to_string()
            } else {
                p.provider.clone()
            };
            let hosts = if p.hosts.is_empty() {
                "—".to_string()
            } else {
                p.hosts.join(", ")
            };
            out.push_str(&format!(
                "<tr><td><code>{}</code></td><td class=\"n\">{}</td>\
                 <td class=\"n\">{}</td><td class=\"n\">{}</td>\
                 <td class=\"n\">{:.1}%</td><td><code>{}</code></td></tr>\n",
                escape(&name),
                num(p.requests),
                num(p.tokens_in),
                num(p.saved),
                p.ratio * 100.0,
                escape(&hosts)
            ));
        }
        out.push_str("</table>\n");
        out.push_str(
            "<p class=\"note\">The provider is the upstream mount's own name, from the \
             configuration — not a host, because one mount can send to more than one. The \
             same <code>openai</code> mount goes to <code>api.openai.com</code> on an API key \
             and to <code>chatgpt.com</code> under a subscription, which is what the last \
             column shows. <code>(unnamed)</code> is traffic recorded before the mount name \
             was, or through a surface that has no mount at all.</p>\n",
        );
    }
    out.push_str("</section>\n");

    let r = &data.retrievals;
    out.push_str(&format!(
        "<section>\n<h2>Recoverability</h2>\n<table>\n\
         <tr><th>asks</th><th>hits</th><th>misses</th><th>withheld</th><th>expired</th>\
         <th>ranged</th><th class=\"n\">tokens back</th></tr>\n\
         <tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} ({:.0}%)</td>\
         <td class=\"n\">{}</td></tr>\n</table>\n\
         <p class=\"note\">A miss is a handle the model asked for and the store could not \
         answer — raise <code>engine.max_ref_bytes</code> or <code>refs.ttl</code> if this \
         is not zero. Misses cost the model an answer; they never cost the request, which is \
         why only hits are counted in <b>tokens back</b> — the figure subtracted from gross \
         to get net.</p>\n\
         <p class=\"note\"><b>Withheld</b> is a different failure and is counted outside \
         <b>asks</b>: the model asked, and <code>proxy.retrieve_max_tokens_per_request</code> \
         was already spent, so the store was never consulted. It is not a miss — the store \
         may well have had the fragment — and it is the only place that budget shows itself. \
         A hit rate of 100% beside a non-zero figure here is a true statement about the store \
         and a misleading one about the request.</p>\n\
         </section>\n",
        num(r.retrievals as i64),
        num(r.hits as i64),
        num(r.misses as i64),
        num(r.withheld as i64),
        num(r.expiries as i64),
        num(r.ranged as i64),
        r.ranged_share * 100.0,
        num(r.retrieved_tokens)
    ));

    if !s.by_mode.is_empty() {
        // Same columns, same order, as the headline cards.
        out.push_str(
            "<section>\n<h2>By mode</h2>\n<table>\n\
                      <tr><th>mode</th><th class=\"n\">requests</th>\
                      <th class=\"n\">tokens in</th><th class=\"n\">net</th>\
                      <th class=\"n\">retrieved</th><th class=\"n\">gross</th></tr>\n",
        );
        for m in &s.by_mode {
            out.push_str(&format!(
                "<tr><td><code>{}</code></td><td class=\"n\">{}</td>\
                 <td class=\"n\">{}</td><td class=\"n\">{}</td>\
                 <td class=\"n\">{}</td><td class=\"n\">{}</td></tr>\n",
                escape(&m.mode),
                num(m.requests as i64),
                num(m.tokens_in),
                num(m.saved_net()),
                num(m.retrieved_tokens),
                num(m.saved_gross)
            ));
        }
        out.push_str("</table>\n");
        out.push_str(
            "<p class=\"note\">Only <code>recoverable</code> mints handles, so it is the \
             only mode that can carry a retrieval debit: for every other mode net is gross. \
             A retrieval is charged to the mode of the request it happened in.</p>\n",
        );
        // The per-mode debits are attributed through the request they happened
        // in. A retrieval on a request that was never compressed -- `kompressor
        // retrieve` from a terminal -- belongs to no mode, and silently
        // vanishing is how a column stops adding up without anyone noticing.
        let attributed: i64 = s.by_mode.iter().map(|m| m.retrieved_tokens).sum();
        if attributed < s.retrieved_tokens {
            out.push_str(&format!(
                "<p class=\"note\">{} of the {} retrieved tokens were fetched on requests \
                 that were not themselves compressed, so they appear in no row above. The \
                 headline figure counts them.</p>\n",
                num(s.retrieved_tokens - attributed),
                num(s.retrieved_tokens)
            ));
        }
        out.push_str("</section>\n");
    }

    out.push_str(FOOTER);
    out.push_str("</body>\n</html>\n");
    out
}

/// A bar chart of net savings per bucket, as inline SVG.
///
/// Drawn by hand rather than with a library for the reason in the module doc.
/// Negative buckets are real and are drawn below the axis: a window where the
/// retrieve loop cost more than compression saved is exactly what someone
/// reading this page needs to see.
fn chart(rows: &[BucketRow]) -> String {
    if rows.is_empty() {
        return "<p class=\"note\">No compressions in this window.</p>\n".to_string();
    }
    let (w, h, pad) = (960.0_f64, 220.0_f64, 24.0_f64);
    let peak = rows
        .iter()
        .map(|r| r.saved_net.unsigned_abs() as f64)
        .fold(1.0_f64, f64::max);
    let any_negative = rows.iter().any(|r| r.saved_net < 0);
    // The axis sits in the middle only when something actually went negative;
    // otherwise the bars get the whole height.
    let axis = if any_negative { h / 2.0 } else { h - pad };
    let span = if any_negative {
        h / 2.0 - pad
    } else {
        h - 2.0 * pad
    };
    let slot = w / rows.len() as f64;
    let bw = (slot * 0.7).clamp(1.0, 48.0);

    let mut svg = format!(
        "<svg class=\"chart\" viewBox=\"0 0 {w} {h}\" role=\"img\" \
         aria-label=\"net tokens saved per bucket\">\n"
    );
    svg.push_str(&format!(
        "<line x1=\"0\" y1=\"{axis:.1}\" x2=\"{w}\" y2=\"{axis:.1}\" class=\"axis\"/>\n"
    ));
    for (i, r) in rows.iter().enumerate() {
        let mag = (r.saved_net.unsigned_abs() as f64) / peak * span;
        let x = i as f64 * slot + (slot - bw) / 2.0;
        let (y, cls) = if r.saved_net < 0 {
            (axis, "neg")
        } else {
            (axis - mag, "pos")
        };
        svg.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{bw:.1}\" height=\"{:.1}\" class=\"{cls}\">\
             <title>{}: {} net</title></rect>\n",
            mag.max(0.5),
            escape(&r.label),
            num(r.saved_net)
        ));
    }
    svg.push_str("</svg>\n");
    svg.push_str(&format!(
        "<p class=\"axislabel\"><span>{}</span><span>peak {} net</span><span>{}</span></p>\n",
        escape(&rows[0].label),
        num(peak as i64),
        escape(&rows[rows.len() - 1].label)
    ));
    svg
}

fn card(label: &str, value: &str, note: &str) -> String {
    format!(
        "<div class=\"card\"><span class=\"label\">{}</span>\
         <span class=\"value\">{}</span><span class=\"foot\">{}</span></div>\n",
        escape(label),
        escape(value),
        escape(note)
    )
}

/// Thousands separators, because these numbers are read by people.
fn num(v: i64) -> String {
    let neg = v < 0;
    let digits = v.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg { format!("-{out}") } else { out }
}

/// `YYYY-MM-DD HH:MM` UTC from epoch milliseconds.
///
/// Hand-rolled because the workspace has no date crate and this is the only
/// place that needs one. Civil-from-days is Howard Hinnant's algorithm.
fn stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// HTML-escape. Content types and labels come from the engine rather than from
/// a caller, but a report is a file someone opens in a browser and this costs
/// nothing.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const FOOTER: &str = "<footer>\n\
<h2>Reading these numbers</h2>\n\
<ul>\n\
<li><b>Net is the number.</b> <code>net = gross − tokens back</code>. Gross counts \
every token the engine removed; the debit is what the retrieve loop put back. Only \
retrievals that <i>found</i> their fragment are charged — a miss returns a short \
&quot;no such fragment&quot; and costs nothing worth counting, so an over-aggressive TTL \
reads as the bug it is rather than as an expense. Net can be negative.</li>\n\
<li><b>Gross over-counts an agent loop.</b> A conversation re-sends its history \
every turn, and compression is credited each time it does. The saving is real \
per request and is not a sum of independent wins.</li>\n\
<li><b>These are estimated tokens, not billed ones.</b> The engine's estimator \
is what the size gate uses. For money, compare the provider's own usage \
figures, which <code>kompressor stats request &lt;id&gt;</code> records \
alongside each request.</li>\n\
<li><b>A type that compresses badly is not a fault.</b> Prose and source code \
are left largely alone by design; see the README's <i>Where the remaining bytes \
are</i>.</li>\n\
</ul>\n</footer>\n";

const CSS: &str = "\
:root{--bg:#fbfbfc;--fg:#1c1f24;--dim:#5d646e;--line:#e2e5ea;--card:#fff;--pos:#3f7d58;--neg:#b8503f;--accent:#4a6fa5}\
@media(prefers-color-scheme:dark){:root{--bg:#15171a;--fg:#e8eaed;--dim:#9aa0a6;--line:#2b2f35;--card:#1d2025;--pos:#6bbf8a;--neg:#e0796a;--accent:#7fa3d6}}\
*{box-sizing:border-box}\
body{margin:0;padding:32px 20px 64px;background:var(--bg);color:var(--fg);\
font:15px/1.55 ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif;\
max-width:1040px;margin-inline:auto}\
h1{font-size:26px;margin:0}h2{font-size:15px;text-transform:uppercase;letter-spacing:.07em;\
color:var(--dim);margin:36px 0 12px;font-weight:600}\
header .sub{color:var(--dim);margin:6px 0 0;font-size:13px}\
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin-top:28px}\
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px 16px;\
display:flex;flex-direction:column;gap:3px}\
.card .label{font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:var(--dim)}\
.card .value{font-size:24px;font-weight:650;font-variant-numeric:tabular-nums}\
.card .foot{font-size:11px;color:var(--dim)}\
table{width:100%;border-collapse:collapse;font-variant-numeric:tabular-nums}\
th{text-align:left;font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:var(--dim);\
font-weight:600;padding:6px 10px 6px 0;border-bottom:1px solid var(--line)}\
td{padding:7px 10px 7px 0;border-bottom:1px solid var(--line)}\
th.n,td.n{text-align:right}th.bar,td.bar{width:42%;padding-right:0}\
td.bar span{display:block;height:8px;border-radius:4px;background:var(--accent);min-width:2px}\
code{font:13px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace}\
.chart{width:100%;height:auto;display:block;margin-top:4px}\
.chart .axis{stroke:var(--line);stroke-width:1}\
.chart .pos{fill:var(--pos)}.chart .neg{fill:var(--neg)}\
.axislabel{display:flex;justify-content:space-between;color:var(--dim);font-size:11px;margin:4px 0 0}\
.note{color:var(--dim);font-size:13px;margin:10px 0 0}\
footer{margin-top:48px;border-top:1px solid var(--line);padding-top:8px}\
footer ul{margin:0;padding-left:18px;color:var(--dim);font-size:13px}\
footer li{margin:6px 0}\
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators_and_negatives() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(1_000), "1,000");
        assert_eq!(num(1_234_567), "1,234,567");
        // Net is allowed to be negative, so the formatter has to be.
        assert_eq!(num(-42_000), "-42,000");
    }

    #[test]
    fn timestamps_are_utc_civil_dates() {
        assert_eq!(stamp(0), "1970-01-01 00:00Z");
        assert_eq!(stamp(1_700_000_000_000), "2023-11-14 22:13Z");
        // A leap day, because the civil-from-days conversion is where that breaks.
        assert_eq!(stamp(1_709_164_800_000), "2024-02-29 00:00Z");
    }

    #[test]
    fn markup_from_the_engine_is_escaped() {
        assert_eq!(escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn the_page_names_the_version_that_rendered_it() {
        let out = page(1_000, 0);
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            out.contains(&format!("by kompressor {version}</p>")),
            "in the header line"
        );
        assert!(
            out.contains(&format!(
                "<meta name=\"generator\" content=\"kompressor {version}\">"
            )),
            "and in the document's metadata"
        );
    }

    #[test]
    fn an_empty_window_renders_a_page_rather_than_a_panic() {
        // The case a cron job hits at 00:05 on a quiet morning.
        let rows: Vec<BucketRow> = Vec::new();
        let out = chart(&rows);
        assert!(out.contains("No compressions"));
        assert!(!out.contains("<svg"));
    }

    /// A page with one knob: the debit. Everything else is fixed so a failure
    /// names the thing that moved.
    fn page(saved_gross: i64, retrieved: i64) -> String {
        let summary = Summary {
            tenant: String::new(),
            from_ms: 0,
            to_ms: 86_400_000,
            requests: 3,
            messages_total: 30,
            messages_compressed: 12,
            tokens_in: 400_000,
            tokens_out: 400_000 - saved_gross,
            saved_gross,
            retrieved_tokens: retrieved,
            saved_net: saved_gross - retrieved,
            retrievals: 9,
            retrieval_hits: 7,
            hit_rate: 7.0 / 9.0,
            by_mode: Vec::new(),
            by_surface: Vec::new(),
        };
        let retrievals = RetrievalSummary {
            retrievals: 9,
            hits: 7,
            misses: 2,
            expiries: 1,
            ranged: 3,
            ranged_share: 3.0 / 9.0,
            hit_rate: 7.0 / 9.0,
            // The same figure the summary debits: both read it from
            // `retrieval_totals`, so the page must not show two numbers.
            retrieved_tokens: retrieved,
            withheld: 0,
            top_refs: Vec::new(),
        };
        render(&ReportData {
            summary,
            buckets: Vec::new(),
            types: Vec::new(),
            providers: Vec::new(),
            retrievals,
            bucket: Bucket::Day,
            generated_ms: 1_700_000_000_000,
        })
    }

    #[test]
    fn the_page_shows_net_as_gross_minus_what_retrieval_put_back() {
        // The debit used to be read from the database and then dropped: the
        // page showed net and gross and no way to check one against the other.
        let out = page(100_000, 30_000);
        assert!(out.contains(">70,000<"), "net is the headline");
        assert!(out.contains(">100,000<"), "gross is still quotable");
        assert!(
            out.contains(">30,000<"),
            "the debit has to appear as a number, not only inside net"
        );
        assert!(
            out.contains("100,000 gross \u{2212} 30,000 retrieved"),
            "the arithmetic is spelled out where net is shown:\n{out}"
        );
        assert!(
            out.contains("tokens back"),
            "and again in the recoverability table"
        );
    }

    /// A page whose By mode table has something in it. `window_debit` is the
    /// headline figure, which may exceed what the rows account for.
    fn page_with_modes(rows: Vec<kompressor_stats::ModeTotals>, window_debit: i64) -> String {
        let saved_gross: i64 = rows.iter().map(|m| m.saved_gross).sum();
        let summary = Summary {
            tenant: String::new(),
            from_ms: 0,
            to_ms: 86_400_000,
            requests: 4,
            messages_total: 30,
            messages_compressed: 12,
            tokens_in: rows.iter().map(|m| m.tokens_in).sum(),
            tokens_out: rows.iter().map(|m| m.tokens_out).sum(),
            saved_gross,
            retrieved_tokens: window_debit,
            saved_net: saved_gross - window_debit,
            retrievals: 9,
            retrieval_hits: 7,
            hit_rate: 7.0 / 9.0,
            by_mode: rows,
            by_surface: Vec::new(),
        };
        let retrievals = RetrievalSummary {
            retrievals: 9,
            hits: 7,
            misses: 2,
            expiries: 1,
            ranged: 3,
            ranged_share: 3.0 / 9.0,
            hit_rate: 7.0 / 9.0,
            retrieved_tokens: window_debit,
            withheld: 0,
            top_refs: Vec::new(),
        };
        render(&ReportData {
            summary,
            buckets: Vec::new(),
            types: Vec::new(),
            providers: Vec::new(),
            retrievals,
            bucket: Bucket::Day,
            generated_ms: 1_700_000_000_000,
        })
    }

    fn mode_row(name: &str, gross: i64, retrieved: i64) -> kompressor_stats::ModeTotals {
        kompressor_stats::ModeTotals {
            mode: name.to_string(),
            requests: 2,
            tokens_in: gross * 2,
            tokens_out: gross,
            saved_gross: gross,
            retrieved_tokens: retrieved,
        }
    }

    #[test]
    fn the_by_mode_table_shows_net_and_the_debit_per_mode() {
        let out = page_with_modes(
            vec![
                mode_row("aggressive", 1_500, 0),
                mode_row("recoverable", 8_000, 400),
            ],
            400,
        );
        assert!(out.contains("<th class=\"n\">net</th>"), "net column");
        assert!(
            out.contains("<th class=\"n\">retrieved</th>"),
            "retrieved column"
        );
        // recoverable: 8,000 gross - 400 = 7,600 net.
        assert!(out.contains(">7,600<"), "recoverable net:\n{out}");
        assert!(out.contains(">400<"), "recoverable debit");
        // A mode that mints no handles reads net == gross rather than blank.
        assert!(
            out.contains(">1,500<"),
            "aggressive gross and net both 1,500"
        );
        // Nothing is unattributed here, so no apology note.
        assert!(
            !out.contains("appear in no row above"),
            "a fully attributed window must not claim otherwise"
        );
    }

    #[test]
    fn a_retrieval_belonging_to_no_mode_is_called_out_rather_than_dropped() {
        // 400 charged to recoverable, 437 in the window: the 37 were fetched on
        // a request nothing compressed. Silently showing 400 would make the
        // column look like it added up when it does not.
        let out = page_with_modes(vec![mode_row("recoverable", 8_000, 400)], 437);
        assert!(
            out.contains("appear in no row above"),
            "the shortfall has to be stated:\n{out}"
        );
        assert!(out.contains("37 of the 437 retrieved tokens"));
    }

    #[test]
    fn the_headline_cards_read_in_the_order_the_arithmetic_does() {
        // tokens in -> net -> retrieved -> gross. The order is the point: it
        // reads as one sentence, ending on the flattering number rather than
        // opening with it. A reshuffle that looks harmless undoes that.
        let out = page(100_000, 30_000);
        let at = |label: &str| {
            out.find(&format!("<span class=\"label\">{label}</span>"))
                .unwrap_or_else(|| panic!("no card labelled {label}"))
        };
        let order = ["tokens in", "tokens net", "retrieved", "gross"];
        let positions: Vec<usize> = order.iter().map(|l| at(l)).collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "cards are out of order: {order:?} landed at {positions:?}"
        );
        // "gross" must not match the "tokens net" footnote that mentions it.
        assert!(at("gross") > at("retrieved"));
        assert!(out.contains(">400,000<"), "tokens in is shown");
    }

    #[test]
    fn a_window_retrieval_cost_more_than_it_saved_reports_a_negative_net() {
        // Not hypothetical: one request retrieving ~28 KB over ten rounds
        // re-injected roughly 67K tokens against about 7K saved. A page that
        // clamped this at zero would be hiding the one result worth seeing.
        let out = page(7_000, 67_000);
        assert!(out.contains(">-60,000<"), "net stays negative:\n{out}");
        assert!(out.contains("7,000 gross \u{2212} 67,000 retrieved"));
        // And the share-of-prompt headline goes with it rather than reading 0%.
        assert!(out.contains("-15.0%"), "net over tokens in is negative too");
    }

    #[test]
    fn a_negative_bucket_is_drawn_below_the_axis() {
        let row = |net: i64| BucketRow {
            bucket_ms: 0,
            label: "2026-09-22".into(),
            requests: 1,
            tokens_in: 100,
            tokens_out: 50,
            saved_gross: 50,
            retrieved_tokens: (50 - net).max(0),
            saved_net: net,
        };
        let out = chart(&[row(100), row(-40)]);
        assert!(out.contains("class=\"pos\""));
        assert!(
            out.contains("class=\"neg\""),
            "a window the retrieve loop cost more than it saved has to be visible"
        );
    }
}
