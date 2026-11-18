use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing_subscriber::EnvFilter;

use zcrawl::config::Config;
use zcrawl::report::{self, Denominator, GroupBy, ReportParams};
use zcrawl::user_agent::{Implementation, NodeVersion};
use zcrawl::{crawler, db, status};

/// Config files tried, in order, when `--config` is not given.
const DEFAULT_CONFIG_PATHS: [&str; 2] = ["zcrawl.toml", "/etc/zcrawl/zcrawl.toml"];

#[derive(Parser)]
#[command(version, about = "Zcash p2p crawler for measuring release adoption")]
struct Cli {
    /// Config file. Default: ./zcrawl.toml, then /etc/zcrawl/zcrawl.toml, then built-in defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Database file; overrides `database` in the config.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl the network in rounds until stopped.
    Run {
        /// Run a single round, then exit.
        #[arg(long)]
        once: bool,
    },
    /// Print the adoption curve of one release as CSV.
    Report {
        #[arg(long, value_enum)]
        implementation: ImplArg,
        /// The release, e.g. 2.6.0.
        #[arg(long)]
        version: NodeVersion,
        /// When the release was published, RFC 3339, e.g. 2026-11-18T14:00:00Z.
        #[arg(long, value_parser = parse_rfc3339)]
        released_at: i64,
        /// Count against nodes of the same implementation, or all reachable nodes.
        #[arg(long, value_enum, default_value_t = DenominatorArg::Implementation)]
        denominator: DenominatorArg,
        /// Count each node, or each IPv4 /24 and IPv6 /48, once.
        #[arg(long, value_enum, default_value_t = GroupByArg::Node)]
        group_by: GroupByArg,
    },
    /// Summarise the last complete round.
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImplArg {
    Zebra,
    Zakura,
}

#[derive(Clone, Copy, ValueEnum)]
enum DenominatorArg {
    Implementation,
    All,
}

#[derive(Clone, Copy, ValueEnum)]
enum GroupByArg {
    Node,
    Subnet,
}

fn parse_rfc3339(s: &str) -> Result<i64, String> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map(|t| t.unix_timestamp())
        .map_err(|e| format!("not an RFC 3339 time ({e}); example: 2026-11-18T14:00:00Z"))
}

fn load_config(cli: &Cli) -> anyhow::Result<Config> {
    let mut config = match &cli.config {
        Some(path) => Config::load(path)?,
        None => match DEFAULT_CONFIG_PATHS
            .iter()
            .map(Path::new)
            .find(|p| p.exists())
        {
            Some(path) => Config::load(path)?,
            None => Config::default(),
        },
    };
    if let Some(db) = &cli.db {
        config.database = db.clone();
    }
    Ok(config)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let default_level = match cli.command {
        Command::Run { .. } => "info",
        _ => "warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    match real_main(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main(cli: Cli) -> anyhow::Result<()> {
    let config = load_config(&cli)?;
    match cli.command {
        Command::Run { once } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting tokio runtime")?;
            rt.block_on(crawler::run(config, once))
        }
        Command::Report {
            implementation,
            version,
            released_at,
            denominator,
            group_by,
        } => {
            let conn = db::open_read_only(&config.database)?;
            let params = ReportParams {
                implementation: match implementation {
                    ImplArg::Zebra => Implementation::Zebra,
                    ImplArg::Zakura => Implementation::Zakura,
                },
                version,
                released_at,
                denominator: match denominator {
                    DenominatorArg::Implementation => Denominator::Implementation,
                    DenominatorArg::All => Denominator::All,
                },
                group_by: match group_by {
                    GroupByArg::Node => GroupBy::Node,
                    GroupByArg::Subnet => GroupBy::Subnet,
                },
            };
            let rep = report::compute(&conn, &params)?;
            let mut out = std::io::stdout().lock();
            report::write(&rep, &mut out)?;
            out.flush()?;
            Ok(())
        }
        Command::Status => {
            let conn = db::open_read_only(&config.database)?;
            let st = status::compute(&conn)?;
            let mut out = std::io::stdout().lock();
            status::write(&st, &mut out)?;
            out.flush()?;
            Ok(())
        }
    }
}
