//! `kompressor stats`: what the service has been doing, read from `stats.db`.
//!
//! Every report here prints net alongside gross. Gross is
//! `Σ(tokens_in - tokens_out)` and it is the number that makes compression look
//! good; net subtracts what retrieval put back, and it can be negative. A
//! caller shown only gross is being shown a number this workspace knows to be
//! wrong, so the table form never prints one without the other.

use std::path::PathBuf;

use kompressor_core::Config;
use kompressor_stats::{Bucket, BucketRow, RetrievalSummary, StatsReader, Summary, TypeCoverage};
use serde_json::json;

use crate::compress::print_json;
use crate::error::{CliError, CliResult};

/// Which report to print.
pub enum Report {
    Summary,
    Histogram {
        bucket: Bucket,
    },
    ByType,
    ByProvider,
    Retrievals,
    Request {
        id: String,
    },
    Store,
    /// One self-contained HTML page, to stdout.
    Html {
        bucket: Bucket,
    },
}

/// Arguments of `kompressor stats`.
pub struct StatsArgs {
    pub report: Report,
    pub stats_path: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub tenant: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub json: bool,
}

/// Open the statistics database this machine has, or say precisely why not.
///
/// The two failures read very differently to someone holding a config that
/// clearly names the file, and used to read the same.
fn open_local(path: &std::path::Path) -> Result<StatsReader, CliError> {
    if !path.exists() {
        return Err(CliError::Runtime(format!(
            "{}: no statistics database. Nothing has recorded yet, or --stats points elsewhere.",
            path.display()
        )));
    }
    let reader = StatsReader::open(path).map_err(|e| unreadable(path, &e.to_string()))?;
    // Opening is not proof of reading. SQLite defers the `-shm` mapping a WAL
    // database needs until the first query, so a reader without write access to
    // the directory opens happily and fails later with "attempt to write a
    // readonly database" — too late to choose another source, and attached to
    // whichever report the caller happened to ask for. One cheap query up front
    // moves that failure to where it can still be answered.
    reader
        .store("")
        .map_err(|e| unreadable(path, &e.to_string()))?;
    Ok(reader)
}

/// The file is there and cannot be read.
fn unreadable(path: &std::path::Path, because: &str) -> CliError {
    CliError::Runtime(format!(
        "{}: {because}. The file exists; on a packaged install it belongs to the \
         service user and is in WAL mode, where even a read-only connection writes \
         `-shm`, so another user is refused. Ask the running service instead, or \
         re-run as the owner.",
        path.display()
    ))
}

/// Where the running service answers, if this configuration starts one.
///
/// The API is nested under `/v1`, which is worth knowing: a request to
/// `/stats/summary` on a listener that also serves the proxy reaches the proxy
/// catch-all and comes back as "no upstream is configured for this path".
fn api_base(config: &Config) -> Option<String> {
    let listen = if !config.server.api_listen.is_empty() {
        config.server.api_listen.clone()
    } else {
        config.server.listen.clone()
    };
    (!listen.is_empty()).then(|| format!("http://{listen}/v1"))
}

/// `{"bucket": "hour", "rows": [...]}`, as the histogram endpoint sends it.
#[derive(serde::Deserialize)]
struct Envelope<T> {
    rows: Vec<T>,
}

/// `{"types": [...]}`, as the coverage endpoint sends it.
#[derive(serde::Deserialize)]
struct Types {
    types: Vec<TypeCoverage>,
}

/// A report comes from the file when it can be read, and from the service that
/// owns it when it cannot.
enum Source {
    Local(StatsReader),
    Remote(String),
}

impl Source {
    fn fetch<T: serde::de::DeserializeOwned>(
        base: &str,
        endpoint: &str,
        query: &[(&str, String)],
    ) -> Result<T, CliError> {
        // The query is appended by hand: `RequestBuilder::query` needs a reqwest
        // feature this workspace does not enable, and every value here is a
        // number or a handle, so the encoding is not the interesting part.
        let mut url = format!("{base}/stats/{endpoint}");
        for (i, (key, value)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(value);
        }
        let what = format!("asking {url}");
        let body = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::runtime("starting a runtime", e))?
            .block_on(async {
                let client = reqwest::Client::new();
                let response = client.get(&url).send().await?;
                let status = response.status();
                let text = response.text().await?;
                Ok::<_, reqwest::Error>((status, text))
            })
            .map_err(|e| CliError::runtime(&what, e))?;
        let (status, text) = body;
        if !status.is_success() {
            return Err(CliError::Runtime(format!(
                "{what}: HTTP {status}. {}",
                text.trim()
            )));
        }
        serde_json::from_str(&text).map_err(|e| CliError::runtime(&what, e))
    }

    fn window(tenant: &str, from: i64, to: i64) -> Vec<(&'static str, String)> {
        let mut q = vec![("from", from.to_string()), ("to", to.to_string())];
        if !tenant.is_empty() {
            q.push(("tenant", tenant.to_string()));
        }
        q
    }

    fn summary(&self, tenant: &str, from: i64, to: i64) -> Result<Summary, CliError> {
        match self {
            Source::Local(r) => r
                .summary(tenant, from, to)
                .map_err(|e| CliError::runtime("reading the summary", e)),
            Source::Remote(base) => Self::fetch(base, "summary", &Self::window(tenant, from, to)),
        }
    }

    fn histogram(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
        bucket: Bucket,
    ) -> Result<Vec<BucketRow>, CliError> {
        match self {
            Source::Local(r) => r
                .histogram(tenant, from, to, bucket)
                .map_err(|e| CliError::runtime("reading the histogram", e)),
            Source::Remote(base) => {
                let mut q = Self::window(tenant, from, to);
                q.push((
                    "bucket",
                    match bucket {
                        Bucket::Hour => "hour",
                        Bucket::Day => "day",
                    }
                    .to_string(),
                ));
                // `{"bucket": …, "rows": [...]}` from the endpoint, bare rows
                // from the reader. Unwrap so both feed the same printing.
                Self::fetch::<Envelope<BucketRow>>(base, "histogram", &q).map(|e| e.rows)
            }
        }
    }

    fn by_type(&self, tenant: &str, from: i64, to: i64) -> Result<Vec<TypeCoverage>, CliError> {
        match self {
            Source::Local(r) => r
                .by_type(tenant, from, to)
                .map_err(|e| CliError::runtime("reading type coverage", e)),
            // The endpoint wraps its rows in `{"types": [...]}`; the reader
            // returns the rows. Unwrap so both hand the printing the same thing.
            Source::Remote(base) => {
                Self::fetch::<Types>(base, "by_type", &Self::window(tenant, from, to))
                    .map(|t| t.types)
            }
        }
    }

    fn retrievals(&self, tenant: &str, from: i64, to: i64) -> Result<RetrievalSummary, CliError> {
        match self {
            Source::Local(r) => r
                .retrievals(tenant, from, to)
                .map_err(|e| CliError::runtime("reading retrievals", e)),
            Source::Remote(base) => {
                Self::fetch(base, "retrievals", &Self::window(tenant, from, to))
            }
        }
    }

    /// Reports the service does not expose over HTTP.
    ///
    /// `by-provider` and the HTML page have no endpoint, and the local file is
    /// the only source for them. Saying which report is unavailable beats a
    /// generic permission error that does not mention that the others work.
    fn only_local(&self, report: &str) -> Result<&StatsReader, CliError> {
        match self {
            Source::Local(r) => Ok(r),
            Source::Remote(base) => Err(CliError::Runtime(format!(
                "{report} is not served over HTTP, and the local database could not be \
                 read. Run it as the user that owns the file, or use a report the \
                 service does expose: summary, histogram, by-type, retrievals ({base})."
            ))),
        }
    }
}

/// Run one statistics report.
pub fn stats(args: StatsArgs) -> CliResult {
    // The window is parsed before anything is opened, so a misspelled `--from`
    // reports itself as the usage error it is even when the database is also
    // missing — the first error a caller sees should be the one they can fix.
    //
    // The default window is all of time. A window that defaulted to "today"
    // would report zeros on a database written yesterday, which reads as "the
    // service is doing nothing".
    let from = parse_instant(args.from.as_deref(), 0)?;
    let to = parse_instant(args.to.as_deref(), i64::MAX)?;
    if to < from {
        return Err(CliError::Usage(format!("--from {from} is after --to {to}")));
    }

    // `--stats` wins; otherwise the configured path, read the same way every
    // other command reads it. Taking `Config::default()` here instead made the
    // command report "no statistics database" against `./data/stats.db` while
    // the running server wrote the path its config named — the one case where
    // this is asked, since a default-configured server is answered by either.
    //
    // The configuration is kept, not just its stats path: when the file cannot
    // be read, `server.listen` is what makes the running service reachable
    // instead.
    let (path, listen) = match args.stats_path {
        // An explicitly named file is taken literally. Falling back to a
        // service for a path the caller chose would answer a different question
        // than the one asked.
        Some(path) => (path, None),
        None => {
            // A packaged install puts its configuration in /etc, which is not on
            // the loader's search path. Without this, `stats` on such a machine
            // finds nothing, falls back to built-in defaults, and reports "no
            // statistics database" against a per-user path the service has
            // never written to — while the file it was told about fills up a
            // directory away.
            //
            // Only this command looks there, and only when nothing nearer was
            // found. Putting it in `Config::discover` instead made six hermetic
            // CLI tests change their output the moment that file existed.
            let explicit = args.config.clone().or_else(|| {
                Config::discover()
                    .is_none()
                    .then(Config::system_path)
                    .flatten()
            });
            let config =
                Config::load(explicit.as_deref()).map_err(|e| CliError::Usage(e.to_string()))?;
            let listen = api_base(&config);
            (config.stats.path, listen)
        }
    };

    let source = match open_local(&path) {
        Ok(reader) => Source::Local(reader),
        Err(local) => match listen {
            // A packaged install puts the database under the service's state
            // directory, owned by the service user. It is WAL, and a read-only
            // SQLite connection still has to touch `-shm`, so another user is
            // refused with "attempt to write a readonly database" even when the
            // file itself is world-readable. The process that owns it serves the
            // same reports over HTTP; ask that instead of explaining chmod.
            Some(base) => Source::Remote(base),
            None => return Err(local),
        },
    };
    let tenant = args.tenant.as_str();

    match args.report {
        Report::Summary => {
            let summary = source.summary(tenant, from, to)?;
            if args.json {
                print_json(&summary)?;
            } else {
                println!("tenant              {}", summary.tenant);
                println!("requests            {}", summary.requests);
                println!(
                    "messages            {} total, {} compressed",
                    summary.messages_total, summary.messages_compressed
                );
                println!(
                    "tokens              {} in, {} out",
                    summary.tokens_in, summary.tokens_out
                );
                println!("saved gross         {}", summary.saved_gross);
                println!(
                    "retrieved back      {} over {} retrievals ({} hits, {:.1}% hit rate)",
                    summary.retrieved_tokens,
                    summary.retrievals,
                    summary.retrieval_hits,
                    summary.hit_rate * 100.0
                );
                println!("saved net           {}", summary.saved_net);
                for mode in &summary.by_mode {
                    println!(
                        "  mode {:<12} {} requests, {} gross",
                        mode.mode, mode.requests, mode.saved_gross
                    );
                }
                for surface in &summary.by_surface {
                    println!(
                        "  from {:<12} {} requests, {} gross",
                        surface.surface, surface.requests, surface.saved_gross
                    );
                }
            }
        }
        Report::Histogram { bucket } => {
            let rows = source
                .histogram(tenant, from, to, bucket)
                .map_err(|e| CliError::runtime("reading the histogram", e))?;
            if args.json {
                print_json(&rows)?;
            } else {
                println!(
                    "{:<22}  {:>8}  {:>12}  {:>12}  {:>12}",
                    "bucket", "requests", "gross", "retrieved", "net"
                );
                for row in &rows {
                    println!(
                        "{:<22}  {:>8}  {:>12}  {:>12}  {:>12}",
                        row.label,
                        row.requests,
                        row.saved_gross,
                        row.retrieved_tokens,
                        row.saved_net
                    );
                }
            }
        }
        Report::ByType => {
            let rows = source
                .by_type(tenant, from, to)
                .map_err(|e| CliError::runtime("reading type coverage", e))?;
            if args.json {
                print_json(&rows)?;
            } else {
                println!("{:<14}  {:>10}  {:>8}", "type", "messages", "share");
                for row in &rows {
                    println!(
                        "{:<14}  {:>10}  {:>7.2}%",
                        row.content_type,
                        row.messages,
                        row.share * 100.0
                    );
                }
            }
        }
        Report::ByProvider => {
            let rows = source
                .only_local("by-provider")?
                .by_provider(tenant, from, to)
                .map_err(|e| CliError::runtime("reading provider coverage", e))?;
            if args.json {
                print_json(&rows)?;
            } else {
                println!(
                    "{:<14}  {:>8}  {:>12}  {:>12}  {:>7}  sent to",
                    "provider", "requests", "tokens in", "removed", "share"
                );
                for row in &rows {
                    let name = if row.provider.is_empty() {
                        "(unnamed)"
                    } else {
                        row.provider.as_str()
                    };
                    println!(
                        "{:<14}  {:>8}  {:>12}  {:>12}  {:>6.2}%  {}",
                        name,
                        row.requests,
                        row.tokens_in,
                        row.saved,
                        row.ratio * 100.0,
                        row.hosts.join(", ")
                    );
                }
            }
        }
        Report::Retrievals => {
            let summary = source
                .retrievals(tenant, from, to)
                .map_err(|e| CliError::runtime("reading retrievals", e))?;
            if args.json {
                print_json(&summary)?;
            } else {
                println!("retrievals     {}", summary.retrievals);
                println!(
                    "hits           {} ({:.1}%)",
                    summary.hits,
                    summary.hit_rate * 100.0
                );
                println!(
                    "misses         {} of which {} expired",
                    summary.misses, summary.expiries
                );
                println!(
                    "ranged         {} ({:.1}%)",
                    summary.ranged,
                    summary.ranged_share * 100.0
                );
                println!("tokens back    {}", summary.retrieved_tokens);
                // Printed whenever it is non-zero, and never folded into the
                // rate above. A hit rate of 100% beside eleven withheld asks is
                // a true statement about the store and a misleading one about
                // the request, and the only cure is to say both.
                if summary.withheld > 0 {
                    println!(
                        "withheld       {} — the per-request token budget refused these \
                         before the store was read; raise proxy.retrieve_max_tokens_per_request",
                        summary.withheld
                    );
                }
                for reference in &summary.top_refs {
                    print!(
                        "  {} {} asks, {} hits, {} tokens",
                        reference.reference,
                        reference.retrievals,
                        reference.hits,
                        reference.returned_tokens
                    );
                    if reference.withheld > 0 {
                        print!(", {} withheld", reference.withheld);
                    }
                    println!();
                }
            }
        }
        Report::Request { id } => {
            let economy = source
                .only_local("request")?
                .request(tenant, &id)
                .map_err(|e| CliError::runtime("reading the request", e))?;
            match economy {
                Some(economy) => {
                    if args.json {
                        print_json(&economy)?;
                    } else {
                        println!("request        {}", economy.request_id);
                        println!("tenant         {}", economy.tenant);
                        println!(
                            "mode           {}",
                            economy.mode.as_deref().unwrap_or("(never compressed)")
                        );
                        println!("compressions   {}", economy.compressions);
                        println!(
                            "tokens         {} in, {} out",
                            economy.tokens_in, economy.tokens_out
                        );
                        println!("saved gross    {}", economy.saved_gross);
                        println!("saved net      {}", economy.saved_net);
                        if let Some(proxy) = &economy.proxy {
                            println!(
                                "provider       {} prompt tokens billed, {} from cache",
                                proxy.prompt_tokens.unwrap_or(0),
                                proxy.cache_read_tokens.unwrap_or(0)
                            );
                        }
                    }
                }
                None => {
                    if args.json {
                        print_json(&json!({"request_id": id, "found": false}))?;
                    }
                    return Err(CliError::Runtime(format!(
                        "no events recorded for request {id} under tenant '{tenant}'"
                    )));
                }
            }
        }
        Report::Html { bucket } => {
            // `--format json` is meaningless for a page, and silently ignoring
            // a flag the caller passed is how they end up wondering why their
            // pipeline got HTML.
            if args.json {
                return Err(CliError::Usage(
                    "stats report renders HTML; --format json is not available for it. The \
                     other reports take --format json and carry the same numbers."
                        .into(),
                ));
            }
            let data = crate::report::ReportData::gather(
                source.only_local("the HTML report")?,
                tenant,
                from,
                to,
                bucket,
                now_ms(),
            )
            .map_err(|e| CliError::runtime("building the report", e))?;
            print!("{}", crate::report::render(&data));
        }
        Report::Store => {
            let sample = source
                .only_local("store")?
                .store(tenant)
                .map_err(|e| CliError::runtime("reading the store sample", e))?;
            match sample {
                Some(sample) => {
                    if args.json {
                        print_json(&json!({
                            "time_ms": sample.time_ms,
                            "live_refs": sample.live_refs,
                            "live_bytes": sample.live_bytes,
                        }))?;
                    } else {
                        println!("sampled at     {}", sample.time_ms);
                        println!("live refs      {}", sample.live_refs);
                        println!("live bytes     {}", sample.live_bytes);
                    }
                }
                None => {
                    if args.json {
                        print_json(&json!({"found": false}))?;
                    } else {
                        println!("no store sample has been recorded yet");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Parse a window bound.
///
/// Three spellings, because the three questions asked of this are different:
/// `1700000000000` when copying a timestamp out of a row, `2026-08-28` when
/// looking at a day, and `7d` when asking what happened recently.
fn parse_instant(value: Option<&str>, default: i64) -> Result<i64, CliError> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(default);
    };

    if let Ok(millis) = value.parse::<i64>() {
        return Ok(millis);
    }
    if let Some(ago) = parse_relative(value) {
        return Ok(now_ms().saturating_sub(ago));
    }
    if let Some(millis) = parse_date(value) {
        return Ok(millis);
    }
    Err(CliError::Usage(format!(
        "'{value}' is not a time: use epoch milliseconds, YYYY-MM-DD, or a span like 7d"
    )))
}

/// `30s`, `15m`, `24h`, `7d` as milliseconds.
fn parse_relative(value: &str) -> Option<i64> {
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    let amount: i64 = digits.parse().ok()?;
    let scale = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    amount.checked_mul(scale)
}

/// `YYYY-MM-DD` as milliseconds since the epoch, UTC midnight.
fn parse_date(value: &str) -> Option<i64> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400_000)
}

/// Days from 1970-01-01 to `y-m-d`, by Howard Hinnant's civil-date algorithm.
///
/// Written out rather than pulled in as a dependency: this is the only date
/// arithmetic in the CLI, and a calendar crate is a large surface to carry for
/// one function whose correctness is checkable against known dates in a test.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Wall clock in milliseconds, or zero if the clock is before the epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_window_bound_takes_its_default() {
        assert_eq!(parse_instant(None, 0).unwrap(), 0);
        assert_eq!(parse_instant(Some("  "), i64::MAX).unwrap(), i64::MAX);
    }

    #[test]
    fn epoch_milliseconds_are_taken_literally() {
        assert_eq!(
            parse_instant(Some("1700000000000"), 0).unwrap(),
            1_700_000_000_000
        );
    }

    #[test]
    fn a_calendar_date_is_utc_midnight() {
        // 2023-11-14T00:00:00Z, checkable against any epoch converter.
        assert_eq!(
            parse_instant(Some("2023-11-14"), 0).unwrap(),
            1_699_920_000_000
        );
        // the epoch itself, and a leap day
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024-02-29"), Some(1_709_164_800_000));
    }

    #[test]
    fn a_span_is_measured_backwards_from_now() {
        let day_ago = parse_instant(Some("1d"), 0).unwrap();
        let delta = now_ms() - day_ago;
        assert!((86_400_000..86_400_000 + 5_000).contains(&delta), "{delta}");
    }

    #[test]
    fn an_unparseable_bound_is_a_usage_error() {
        assert_eq!(
            parse_instant(Some("last tuesday"), 0).unwrap_err().code(),
            2
        );
    }
}
