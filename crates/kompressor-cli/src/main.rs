//! The kompressor command line.
//!
//! Every subcommand here runs offline: it needs no server, no network and no
//! credential. That is the point of having them — the engine's behaviour on a
//! payload should be answerable without deploying anything, and the two
//! questions an operator actually asks (*what did it do to this, and why?*)
//! are one command each.
//!
//! Exit codes are part of the interface: 0 success, 1 a runtime failure, 2 a
//! usage or configuration error, 3 a benchmark comparison that regressed.

#![forbid(unsafe_code)]

mod backup;
mod bench;
mod compress;
mod db;
mod doctor;
mod error;
mod explain;
mod highlight;
mod hook;
mod input;
mod report;
mod restore;
mod retrieve;
mod serve;
mod stats;
mod wrap;

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};
use kompressor_core::Config;
use kompressor_engine::types::{ForcedType, Mode};
use kompressor_stats::Bucket;

use crate::error::{CliError, CliResult};

#[derive(Parser)]
#[command(
    name = "kompressor",
    version,
    about = "OptScale Kompressor — deterministic prompt compression: compress, explain, retrieve, measure.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compress one payload and print the result.
    Compress(CompressCmd),
    /// Classify one payload without compressing it.
    Detect(DetectCmd),
    /// Fetch back a fragment a compression elided.
    Retrieve(RetrieveCmd),
    /// Report what the service has been doing.
    Stats(StatsCmd),
    /// Replay a corpus through the engine.
    Bench(BenchCmd),
    /// Load a configuration, validate it, and print it with secrets removed.
    CheckConfig(CheckConfigCmd),
    /// Check this setup: config, proxy listener, stores, agents, base-URL
    /// variables. Exit 0 healthy, 1 warnings, 2 failures.
    Doctor(DoctorCmd),
    /// Write every database to one .tar.gz, without stopping the service.
    Backup(BackupCmd),
    /// Put the databases in a backup back. The service must be stopped.
    Restore(RestoreCmd),
    /// Describe, check or checkpoint the databases, without stopping the service.
    #[command(subcommand)]
    Db(DbCmd),
    /// Run the HTTP API, the MCP tool server and the reverse proxy.
    Serve(ServeCmd),
    /// Serve the tools to an agent runtime that launches this process itself.
    #[command(subcommand)]
    Mcp(McpCmd),
    /// Run a coding agent with its API traffic compressed.
    Wrap(WrapCmd),
    /// Undo a `wrap --attach`, restoring the agent's settings file.
    Unwrap(UnwrapCmd),
    /// Hook mode: compress an agent's tool output from the agent's own hooks.
    #[command(subcommand)]
    Hook(HookCmd),
    /// Print the version and the compiled-in features.
    Version,
}

#[derive(Args)]
struct WrapCmd {
    /// Agent to run. Omit it and pass --env with a command after `--`.
    agent: Option<String>,
    /// Arguments for the agent, after `--`. With no agent named, the first of
    /// these is the command to run.
    #[arg(last = true, allow_hyphen_values = true)]
    args: Vec<String>,
    /// Port for the proxy. Defaults to the configured listener.
    #[arg(long, short = 'p')]
    port: Option<u16>,
    /// Configuration the proxy runs under.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Compression mode for this session, overriding the configuration.
    #[arg(long)]
    mode: Option<String>,
    /// Require a proxy to be running already rather than starting one.
    #[arg(long)]
    no_proxy: bool,
    /// Configure an agent this command does not launch, and hold the proxy up
    /// until interrupted. For editor-hosted agents that never see our
    /// environment.
    #[arg(long)]
    attach: bool,
    /// Print the environment instead of running anything, for `eval $(...)`.
    #[arg(long)]
    print: bool,
    /// Point an unlisted tool at the proxy: NAME=root or NAME=v1, repeatable.
    #[arg(long = "env", value_name = "NAME=SHAPE")]
    env: Vec<String>,
    /// List the agents this command knows.
    #[arg(long)]
    list: bool,
}

/// `kompressor hook`: the commands an agent's hooks run, and the ones that
/// register them. See docs/CURSOR.md.
#[derive(Subcommand)]
enum HookCmd {
    /// Answer one Cursor hook event: JSON on stdin, one JSON reply on stdout.
    /// Always exits 0; any failure is the empty reply.
    Cursor(HookCursorCmd),
    /// Print a captured command output compressed, delete it, exit with
    /// --status. The tail of a rewritten shell command.
    Emit(HookEmitCmd),
    /// Register the hooks and the retrieval server with an agent.
    Install {
        #[command(subcommand)]
        agent: HookInstallAgent,
    },
    /// Remove exactly what `hook install` wrote.
    Uninstall {
        #[command(subcommand)]
        agent: HookScopeAgent,
    },
    /// What is installed, what else answers the same events, whether the
    /// store is reachable, and when a hook last ran.
    Status {
        #[command(subcommand)]
        agent: HookStatusAgent,
    },
}

#[derive(Args)]
struct HookCursorCmd {
    /// Configuration file. Installs and `wrap` pass an absolute one, so every
    /// hook process opens the same store.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Measure only, whatever the configuration says.
    #[arg(long)]
    shadow: bool,
}

#[derive(Args)]
struct HookEmitCmd {
    /// The captured output.
    file: PathBuf,
    /// The exit status of the command that produced it; this command exits
    /// with it on every path.
    #[arg(long, allow_hyphen_values = true)]
    status: i32,
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// The agent's generation id, for the statistics row.
    #[arg(long)]
    request: Option<String>,
    /// The agent's conversation id, for the statistics row.
    #[arg(long)]
    conversation: Option<String>,
    #[arg(long)]
    model: Option<String>,
}

#[derive(Subcommand)]
enum HookInstallAgent {
    /// Cursor, desktop and CLI: ~/.cursor/hooks.json and ~/.cursor/mcp.json.
    Cursor(HookInstallCursorCmd),
}

#[derive(Args)]
struct HookInstallCursorCmd {
    /// Install into the current workspace's .cursor directory instead of ~/.cursor.
    #[arg(long)]
    project: bool,
    /// Print the entries as JSON and write nothing (for the enterprise path).
    #[arg(long)]
    print: bool,
    /// Measure only: record what would be removed, change nothing.
    #[arg(long)]
    shadow: bool,
    /// Install beside another tool's preToolUse hook on Shell.
    #[arg(long)]
    force: bool,
    /// Add Shell(kompressor) to the Cursor CLI's allow-list.
    #[arg(long)]
    allow_list: bool,
    /// Configuration the hooks run under.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum HookScopeAgent {
    Cursor {
        /// The install in the current workspace's .cursor directory.
        #[arg(long)]
        project: bool,
    },
}

#[derive(Subcommand)]
enum HookStatusAgent {
    Cursor {
        #[arg(long)]
        project: bool,
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
}

#[derive(Args)]
struct UnwrapCmd {
    /// Agent whose settings file should be restored.
    agent: String,
}

/// The tool server, over the transport an agent runtime spawns rather than
/// dials.
///
/// `serve --mcp` already offers the same tools over HTTP. This exists because
/// the runtimes that matter here — Claude Code among them — do not dial a URL
/// for local tools; they start a process and speak JSON-RPC down its pipe.
#[derive(Subcommand)]
enum McpCmd {
    /// Speak MCP over stdin and stdout until the runtime disconnects.
    Serve(McpServeCmd),
}

#[derive(Args)]
struct McpServeCmd {
    /// Configuration file. This decides which fragment store is opened, so it
    /// must name the same store as the proxy that minted the handles — which
    /// is what `kompressor wrap` passes when it registers this server.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct ServeCmd {
    /// Configuration file. Without one the defaults are a working
    /// single-tenant deployment.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Address to bind, overriding the configured one.
    #[arg(long, value_name = "ADDR")]
    listen: Option<String>,
    /// Serve the HTTP compression API.
    #[arg(long)]
    api: bool,
    /// Serve the MCP tool endpoint.
    #[arg(long)]
    mcp: bool,
    /// Serve the LLM reverse proxy.
    #[arg(long)]
    proxy: bool,
    /// Stop when a `stop` line arrives on standard input. Passed by
    /// `kompressor wrap`, which holds the other end of the pipe.
    #[arg(long, hide = true)]
    stop_on_stdin: bool,
    /// Once standard input closes, stop as soon as no live client marker is
    /// left in this directory. Passed by `kompressor wrap` with
    /// `--stop-on-stdin`, so a proxy whose owner is gone does not run for ever.
    #[arg(long, hide = true, value_name = "DIR", requires = "stop_on_stdin")]
    stop_when_unused: Option<std::path::PathBuf>,
}

#[derive(Args)]
struct CompressCmd {
    /// Payload to compress; `-` or omitted reads standard input.
    file: Option<String>,
    /// Configuration to take engine options from. Without it the usual search
    /// path and `KOMPRESSOR__*` environment apply, so this command compresses
    /// the way the server would.
    #[arg(long)]
    config: Option<PathBuf>,
    /// aggressive, lossless or recoverable.
    #[arg(long, default_value = "recoverable", value_parser = parse_mode)]
    mode: Mode,
    /// Role the payload arrived as. A protected role is left untouched.
    #[arg(long, default_value = "tool")]
    role: String,
    /// Force a content type instead of detecting one: json, log, text, config.
    #[arg(long = "type", value_parser = parse_forced_type)]
    forced_type: Option<ForcedType>,
    /// Print one JSON object instead of the payload.
    #[arg(long)]
    json: bool,
    /// Explain every decision. Without --json this goes to standard error, so
    /// the compressed payload on standard output stays pipeable.
    #[arg(long)]
    explain: bool,
    /// Also show the compressed payload on standard error, numbered and
    /// highlighted: omission markers and their handles, plus JSON, log, diff,
    /// search, code and config structure. Standard output is unchanged.
    #[arg(long)]
    show_output: bool,
    /// Colour on standard error: auto (a terminal, unless NO_COLOR or
    /// TERM=dumb), always or never. Standard output is never coloured.
    #[arg(long, value_enum, default_value_t = highlight::ColorWhen::Auto)]
    color: highlight::ColorWhen,
}

#[derive(Args)]
struct DetectCmd {
    /// Payload to classify; `-` or omitted reads standard input.
    file: Option<String>,
    /// Configuration to take engine options from; detection depends on several.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, default_value = "tool")]
    role: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
#[command(group(ArgGroup::new("window").args(["lines", "pointer", "grep"])))]
struct RetrieveCmd {
    /// The handle, with or without its `k_` prefix.
    reference: String,
    /// Fragment store to read. Defaults to the configured refs path.
    #[arg(long = "refs")]
    refs_path: Option<PathBuf>,
    /// Configuration to take the refs path from. Without it the usual search
    /// path and `KOMPRESSOR__*` environment apply, so this reads the store the
    /// `compress` that minted the handle wrote.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Tenant the handle belongs to; the default is the anonymous tenant.
    #[arg(long, default_value = "")]
    tenant: String,
    /// A 1-based inclusive line window, as START:END.
    #[arg(long)]
    lines: Option<String>,
    /// An RFC 6901 pointer into a fragment that is JSON.
    #[arg(long)]
    pointer: Option<String>,
    /// Lines matching a pattern, with two lines of context.
    #[arg(long)]
    grep: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StatsCmd {
    #[command(subcommand)]
    report: StatsReport,
    /// Statistics database to read. Defaults to the configured stats path.
    #[arg(long = "stats", global = true)]
    stats_path: Option<PathBuf>,
    /// Configuration to take the stats path from. Without it the usual search
    /// path and `KOMPRESSOR__*` environment apply, so this reads the database
    /// the server it is asked about is writing.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true, default_value = "")]
    tenant: String,
    /// Window start: epoch milliseconds, YYYY-MM-DD, or a span like 7d.
    #[arg(long, global = true)]
    from: Option<String>,
    /// Window end, same spellings. The default window is all of time.
    #[arg(long, global = true)]
    to: Option<String>,
    /// table or json.
    #[arg(long, global = true, default_value = "table", value_parser = ["table", "json"])]
    format: String,
}

#[derive(Subcommand)]
enum StatsReport {
    /// Requests, tokens, and savings gross and net.
    Summary,
    /// Savings over time.
    Histogram {
        /// hour or day.
        #[arg(long, default_value = "hour", value_parser = parse_bucket)]
        bucket: Bucket,
    },
    /// Messages per detected content type.
    ByType,
    /// Requests and savings per upstream mount, and the hosts it sent to.
    ByProvider,
    /// Hook mode (Cursor): results per agent tool, removed and shadow-measured.
    ByTool,
    /// The retrieval side: hits, expiries, tokens put back.
    Retrievals,
    /// One request's economy, compression and retrievals joined.
    Request {
        /// The request id.
        id: String,
    },
    /// The most recent fragment-store sample.
    Store,
    /// What the provider billed and what its prompt cache saved, by model:
    /// fresh input, cache reads, five-minute and one-hour writes, output, and
    /// the input priced in input-equivalent tokens against an uncached
    /// baseline. Keep-warm pings are counted apart.
    Cache,
    /// Why messages were left uncompressed, ranked by the tokens they hold,
    /// and how each engine run ended (compressed, discarded, timed out...).
    Skips,
    /// One row per conversation: turns, pings, cache use, cost, engine credit
    /// and retrieval debit.
    Conversations {
        /// Rows to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// cost, turns, saved (least net saving first) or recent.
        #[arg(long, default_value = "cost", value_parser = ["cost", "turns", "saved", "recent"])]
        sort: String,
    },
    /// One self-contained HTML page: headline, trend, types, retrievals.
    ///
    /// Written to stdout, so `kompressor stats report > report.html`. No
    /// JavaScript and no external assets — it renders from `file://` on a
    /// machine with no network.
    Report {
        /// Bucket the trend chart by hour or day.
        #[arg(long, default_value = "day", value_parser = parse_bucket)]
        bucket: Bucket,
    },
}

#[derive(Args)]
struct BenchCmd {
    #[command(subcommand)]
    action: BenchAction,
}

#[derive(Subcommand)]
enum BenchAction {
    /// Replay a corpus and score what came back.
    Replay {
        /// Corpus file, JSON lines, gzipped or plain.
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, default_value = "recoverable", value_parser = parse_mode)]
        mode: Mode,
        /// Record this run as a baseline.
        #[arg(long)]
        save: Option<PathBuf>,
        /// Check this run against a recorded baseline; exits 3 on a regression.
        #[arg(long)]
        compare: Option<PathBuf>,
        /// Re-compress every payload to prove the bytes are stable.
        #[arg(long)]
        check_determinism: bool,
        #[arg(long)]
        json: bool,
        /// Configuration whose `[engine]` section the replay runs under.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Price a conversation corpus under the fixed elision gate and the
    /// net-cost one: removal value minus expected fetch cost, per provider.
    Price {
        /// Conversation corpus (`scripts/bench/extract-conversations.py`).
        #[arg(long)]
        conversations: PathBuf,
        /// Configuration whose `[engine]` and `[proxy.net_cost_gate]` the
        /// replay runs under.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// The fixed arm's `min_elision_saving_chars`.
        #[arg(long, default_value_t = 4000)]
        fixed_chars: usize,
        /// Tokens billed outside the messages (system prompt, tool schemas),
        /// which a corpus does not record and every fetch re-reads.
        #[arg(long, default_value_t = 0)]
        outside_tokens: u32,
        #[arg(long)]
        json: bool,
    },
    /// Build a corpus from what real runs elided, so floors track real traffic.
    Extract {
        /// Run dumps to walk, or `kompressor-refs.db` files directly.
        #[arg(long, value_name = "PATH", required = true, num_args = 1..)]
        from: Vec<PathBuf>,
        /// Corpus to write; gzipped when the name ends in `.gz`.
        #[arg(long)]
        out: PathBuf,
        /// Drop fragments shorter than this.
        #[arg(long, default_value_t = 200)]
        min_bytes: usize,
        /// Drop fragments containing this text; repeatable. Use it to keep a
        /// harness's own self-test payloads out of a corpus of real traffic.
        #[arg(long, value_name = "TEXT")]
        exclude: Vec<String>,
        /// Task label recorded on every payload.
        #[arg(long)]
        task: Option<String>,
        /// Drop fragments whose control-character share exceeds this: a
        /// binary file an agent dumped is not the tool text a corpus is for.
        #[arg(long, default_value_t = 0.01)]
        max_control_ratio: f64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct RestoreCmd {
    /// The archive `kompressor backup` or the Maintenance page wrote.
    archive: PathBuf,
    /// The configuration naming where the databases are kept.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Restore one database, `stats.db` or `refs.db`, and leave the other.
    #[arg(long, value_name = "NAME")]
    only: Option<String>,
    /// Do it. Without this the archive is checked and what would happen is
    /// printed; nothing is changed.
    #[arg(long)]
    yes: bool,
    /// Print the outcome as one JSON object instead of a summary.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum DbCmd {
    /// Size, write-ahead log, schema version and reclaimable space of each.
    Status(DbArgs),
    /// Run a full integrity check. Exits 1 if a database is damaged.
    Check(DbArgs),
    /// Fold the write-ahead log into the database file.
    Checkpoint(DbArgs),
}

#[derive(Args)]
struct DbArgs {
    /// One database, `stats.db` or `refs.db`. Both when omitted.
    name: Option<String>,
    /// The configuration naming the databases.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Print one JSON array instead of a line per database.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct BackupCmd {
    /// Where to write the archive: a file to create, or a directory to put a
    /// timestamped one in. Defaults to the current directory. An existing
    /// file is never replaced.
    #[arg(long, short = 'o', value_name = "PATH")]
    out: Option<PathBuf>,
    /// The configuration naming the databases to back up.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Print what was written as one JSON object instead of a summary.
    #[arg(long)]
    json: bool,
    /// After writing into a directory, keep only this many archives there
    /// and remove the older ones. Only files named as this command names
    /// them are ever removed.
    #[arg(long, value_name = "N")]
    keep: Option<u32>,
}

#[derive(Args)]
struct DoctorCmd {
    /// Configuration file, found the way every other command finds it when
    /// omitted.
    #[arg(long)]
    config: Option<PathBuf>,
    /// One JSON object: `{version, exit_code, checks: [{check, status,
    /// detail, remedy}]}`.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CheckConfigCmd {
    /// Configuration file. Without one the built-in defaults are validated,
    /// which is a working single-tenant deployment.
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        // The agent's own failure is not ours to announce.
        if !matches!(error, CliError::AgentExit(_) | CliError::Quiet(_)) {
            eprintln!("kompressor: {error}");
        }
        std::process::exit(error.code());
    }
}

fn run(cli: Cli) -> CliResult {
    match cli.command {
        Command::Compress(args) => compress::compress(compress::CompressArgs {
            file: args.file,
            config: args.config,
            mode: args.mode,
            role: args.role,
            forced: args.forced_type,
            json: args.json,
            explain: args.explain,
            show_output: args.show_output,
            color: args.color,
        }),
        Command::Detect(args) => compress::detect(compress::DetectArgs {
            file: args.file,
            config: args.config,
            role: args.role,
            json: args.json,
        }),
        Command::Retrieve(args) => retrieve::retrieve(retrieve::RetrieveArgs {
            reference: args.reference,
            refs_path: args.refs_path,
            config: args.config,
            tenant: args.tenant,
            lines: args.lines,
            pointer: args.pointer,
            grep: args.grep,
            json: args.json,
        }),
        Command::Stats(args) => {
            let json = args.format == "json";
            stats::stats(stats::StatsArgs {
                report: match args.report {
                    StatsReport::Summary => stats::Report::Summary,
                    StatsReport::Histogram { bucket } => stats::Report::Histogram { bucket },
                    StatsReport::ByType => stats::Report::ByType,
                    StatsReport::ByProvider => stats::Report::ByProvider,
                    StatsReport::ByTool => stats::Report::ByTool,
                    StatsReport::Retrievals => stats::Report::Retrievals,
                    StatsReport::Request { id } => stats::Report::Request { id },
                    StatsReport::Store => stats::Report::Store,
                    StatsReport::Cache => stats::Report::Cache,
                    StatsReport::Skips => stats::Report::Skips,
                    StatsReport::Conversations { limit, sort } => stats::Report::Conversations {
                        limit,
                        order: match sort.as_str() {
                            "turns" => kompressor_stats::breakdown::ConversationOrder::Turns,
                            "saved" => kompressor_stats::breakdown::ConversationOrder::Saved,
                            "recent" => kompressor_stats::breakdown::ConversationOrder::Recent,
                            _ => kompressor_stats::breakdown::ConversationOrder::Cost,
                        },
                    },
                    StatsReport::Report { bucket } => stats::Report::Html { bucket },
                },
                stats_path: args.stats_path,
                config: args.config,
                tenant: args.tenant,
                from: args.from,
                to: args.to,
                json,
            })
        }
        Command::Bench(args) => match args.action {
            BenchAction::Replay {
                corpus,
                mode,
                save,
                compare,
                check_determinism,
                json,
                config,
            } => bench::replay_corpus(bench::ReplayArgs {
                corpus,
                mode,
                config,
                save,
                compare,
                check_determinism,
                json,
            }),
            BenchAction::Price {
                conversations,
                config,
                fixed_chars,
                outside_tokens,
                json,
            } => bench::price_corpus(bench::PriceArgs {
                conversations,
                config,
                fixed_chars,
                outside_tokens,
                json,
            }),
            BenchAction::Extract {
                from,
                out,
                min_bytes,
                exclude,
                task,
                max_control_ratio,
                json,
            } => bench::extract_corpus(bench::ExtractArgs {
                from,
                out,
                min_bytes,
                exclude,
                task,
                max_control_ratio,
                json,
            }),
        },
        Command::Wrap(args) => wrap::wrap(wrap::WrapArgs {
            agent: args.agent,
            args: args.args,
            port: args.port,
            config: args.config,
            mode: args.mode,
            no_proxy: args.no_proxy,
            attach: args.attach,
            print: args.print,
            env: args.env,
            list: args.list,
        }),
        Command::Unwrap(args) => wrap::unwrap_agent(wrap::UnwrapArgs { agent: args.agent }),
        Command::Hook(HookCmd::Cursor(args)) => hook::run_cursor(hook::CursorArgs {
            config: args.config,
            shadow: args.shadow,
        }),
        Command::Hook(HookCmd::Emit(args)) => hook::run_emit(hook::EmitArgs {
            file: args.file,
            status: args.status,
            config: args.config,
            request: args.request,
            conversation: args.conversation,
            model: args.model,
        }),
        Command::Hook(HookCmd::Install {
            agent: HookInstallAgent::Cursor(args),
        }) => hook::install::install(hook::install::InstallArgs {
            project: args.project,
            print: args.print,
            shadow: args.shadow,
            force: args.force,
            allow_list: args.allow_list,
            config: args.config,
        }),
        Command::Hook(HookCmd::Uninstall {
            agent: HookScopeAgent::Cursor { project },
        }) => hook::install::uninstall(project),
        Command::Hook(HookCmd::Status {
            agent: HookStatusAgent::Cursor { project, config },
        }) => hook::install::status(project, config.as_deref()),
        Command::CheckConfig(args) => check_config(args.config.as_deref()),
        Command::Doctor(args) => doctor::run(args.config.as_deref(), args.json),
        Command::Backup(args) => backup::run(
            args.config.as_deref(),
            args.out.as_deref(),
            args.json,
            args.keep,
        ),
        Command::Restore(args) => restore::run(
            &args.archive,
            args.config.as_deref(),
            args.only.as_deref(),
            args.yes,
            args.json,
        ),
        Command::Db(DbCmd::Status(args)) => {
            db::status(args.config.as_deref(), args.name.as_deref(), args.json)
        }
        Command::Db(DbCmd::Check(args)) => {
            db::check(args.config.as_deref(), args.name.as_deref(), args.json)
        }
        Command::Db(DbCmd::Checkpoint(args)) => {
            db::checkpoint(args.config.as_deref(), args.name.as_deref(), args.json)
        }
        Command::Serve(args) => {
            let config =
                Config::load(args.config.as_deref()).map_err(|e| CliError::Usage(e.to_string()))?;
            let surfaces = serve::Surfaces::resolve(args.api, args.mcp, args.proxy);
            // The offline subcommands are synchronous by design, so the runtime
            // is built here rather than wrapping `main`: nothing else pays for it.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::runtime("could not start the runtime", e))?;
            runtime.block_on(serve::run(
                config,
                surfaces,
                args.listen,
                serve::Supervision {
                    stop_on_stdin: args.stop_on_stdin,
                    stop_when_unused: args.stop_when_unused,
                },
            ))
        }
        Command::Mcp(McpCmd::Serve(args)) => {
            let config =
                Config::load(args.config.as_deref()).map_err(|e| CliError::Usage(e.to_string()))?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::runtime("could not start the runtime", e))?;
            runtime.block_on(serve::run_stdio(config))
        }
        Command::Version => {
            println!("kompressor {}", env!("CARGO_PKG_VERSION"));
            println!(
                "code-aware (tree-sitter grammars): {}",
                cfg!(feature = "code-aware")
            );
            Ok(())
        }
    }
}

/// Load a configuration and print it back with secrets removed.
///
/// An invalid file exits 2 rather than 1: the run never started, and the fix is
/// in the file rather than in the environment it was going to run against.
fn check_config(path: Option<&std::path::Path>) -> CliResult {
    // A subscriber first, because the loader's warnings are the point of this
    // command and `tracing` compiles to nothing without one. `check-config`
    // installed none, so an override it *refused* — an unknown key, or a value
    // that is not the boolean it needs — printed a configuration that looked
    // correct and said nothing about the setting it had dropped. That is the
    // "accepted and silently ignored" failure this project has already paid
    // four Harbor runs for, met in the one command whose job is to catch it.
    //
    // To stderr, so the TOML on stdout stays pipeable, and at `warn` unless
    // `RUST_LOG` asks for more.
    {
        use tracing_subscriber::filter::EnvFilter;
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .without_time()
            .with_target(false)
            .try_init();
    }
    let config = Config::load(path).map_err(|e| CliError::Usage(e.to_string()))?;
    print!("{}", config.redacted_toml());
    Ok(())
}

/// `--mode`, with the error naming what was accepted.
fn parse_mode(value: &str) -> Result<Mode, String> {
    value.parse()
}

/// `--type`, the caller asserting what a payload is.
fn parse_forced_type(value: &str) -> Result<ForcedType, String> {
    value.parse()
}

/// `--bucket`, the histogram's granularity.
fn parse_bucket(value: &str) -> Result<Bucket, String> {
    match value {
        "hour" => Ok(Bucket::Hour),
        "day" => Ok(Bucket::Day),
        other => Err(format!("unknown bucket: {other}; use hour or day")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        // Catches the clap misconfigurations that are otherwise only found by
        // running the binary: duplicate flags, a global on a non-global arg, an
        // argument group naming something that does not exist.
        Cli::command().debug_assert();
    }

    #[test]
    fn a_mode_name_that_does_not_exist_is_refused() {
        assert!(parse_mode("agressive").is_err());
        assert_eq!(parse_mode("lossless").unwrap(), Mode::Lossless);
    }

    #[test]
    fn the_histogram_granularity_is_hour_or_day() {
        assert!(parse_bucket("week").is_err());
        assert_eq!(parse_bucket("day").unwrap(), Bucket::Day);
    }
}
