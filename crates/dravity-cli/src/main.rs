//! `dravity`: download torrents from the command line.
//!
//! The CLI only sends session commands and renders session alerts; the
//! download loop lives in `dravity-session`.

mod format;

use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dravity_core::{Magnet, Metainfo};
use dravity_engine::{Alert, TorrentStatus};
use dravity_session::{AddTorrentOptions, Session, SessionConfig};
use tokio::sync::broadcast::error::RecvError;

/// A portable BitTorrent client.
#[derive(Debug, Parser)]
#[command(name = "dravity", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download a torrent and exit when it is complete.
    Download {
        /// Path to a `.torrent` file, or a `magnet:` link.
        torrent: String,
        /// Download directory.
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// Extra peer to connect to (repeatable), e.g. `10.0.0.2:6881`.
        #[arg(long = "peer", value_name = "ADDR")]
        peers: Vec<SocketAddr>,
        /// Port reported to trackers.
        #[arg(long, default_value_t = 6881)]
        port: u16,
        /// Maximum connected peers.
        #[arg(long, default_value_t = 50)]
        max_peers: usize,
        /// Download rate limit in bytes per second; accepts `K`/`M`/`G`
        /// suffixes (binary), e.g. `500K` or `2M`.
        #[arg(long, value_name = "RATE", value_parser = format::parse_rate)]
        download_limit: Option<u64>,
        /// Do not use the DHT for peer discovery.
        #[arg(long)]
        no_dht: bool,
        /// Directory for fastresume data (defaults to OUTPUT/.dravity).
        #[arg(long, value_name = "DIR", conflicts_with = "no_resume")]
        resume_dir: Option<PathBuf>,
        /// Neither read nor write fastresume data (always check existing files).
        #[arg(long)]
        no_resume: bool,
        /// Suppress the progress line.
        #[arg(short, long)]
        quiet: bool,
    },
    /// Show what a `.torrent` file or `magnet:` link contains.
    Info {
        /// Path to a `.torrent` file, or a `magnet:` link.
        torrent: String,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DRAVITY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// A torrent given on the command line.
#[derive(Debug)]
enum Input {
    File(Box<Metainfo>),
    Magnet(Magnet),
}

impl Input {
    /// A display name: the torrent name, the magnet's `dn`, or the info-hash.
    fn name(&self) -> String {
        match self {
            Self::File(m) => m.info().name().to_owned(),
            Self::Magnet(m) => m
                .display_name
                .clone()
                .or_else(|| m.info_hash.map(|h| h.to_hex()))
                .unwrap_or_else(|| "magnet".to_owned()),
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Info { torrent } => match load(&torrent)? {
            Input::File(m) => info(&m),
            Input::Magnet(m) => magnet_info(&m),
        },
        Command::Download {
            torrent,
            output,
            peers,
            port,
            max_peers,
            download_limit,
            no_dht,
            resume_dir,
            no_resume,
            quiet,
        } => {
            let input = load(&torrent)?;
            let mut config = SessionConfig::default();
            config.announce_port = port;
            config.engine.max_peers = max_peers.max(1);
            config.engine.download_rate_limit = download_limit.filter(|&r| r > 0);
            if no_dht {
                config.dht = None;
            }
            config.resume_dir = if no_resume {
                None
            } else {
                Some(resume_dir.unwrap_or_else(|| output.join(".dravity")))
            };
            let runtime = tokio::runtime::Runtime::new().context("starting the Tokio runtime")?;
            runtime.block_on(download(input, output, peers, config, quiet))
        }
    }
}

fn load(arg: &str) -> Result<Input> {
    if arg.starts_with("magnet:") {
        let magnet = Magnet::parse(arg).context("parsing the magnet link")?;
        return Ok(Input::Magnet(magnet));
    }
    let path = Path::new(arg);
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let metainfo =
        Metainfo::from_bytes(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Input::File(Box::new(metainfo)))
}

fn magnet_info(m: &Magnet) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if let Some(name) = &m.display_name {
        writeln!(out, "name:       {name}")?;
    }
    if let Some(hash) = m.info_hash {
        writeln!(out, "info-hash:  {hash}")?;
    }
    if let Some(hash) = &m.info_hash_v2 {
        writeln!(out, "v2 hash:    {}", hash.to_hex())?;
    }
    if let Some(len) = m.exact_length {
        writeln!(out, "size:       {} ({len} bytes)", format::bytes(len))?;
    }
    for url in &m.trackers {
        writeln!(out, "tracker:    {url}")?;
    }
    for peer in &m.peers {
        writeln!(out, "peer:       {peer}")?;
    }
    writeln!(out, "(files are known once the metadata is fetched)")?;
    Ok(())
}

fn info(m: &Metainfo) -> Result<()> {
    let i = m.info();
    let mut out = std::io::stdout().lock();
    writeln!(out, "name:       {}", i.name())?;
    writeln!(out, "info-hash:  {}", m.info_hash())?;
    writeln!(
        out,
        "size:       {} ({} bytes)",
        format::bytes(i.total_length()),
        i.total_length()
    )?;
    writeln!(
        out,
        "pieces:     {} x {}",
        i.piece_count(),
        format::bytes(u64::from(i.piece_length()))
    )?;
    writeln!(
        out,
        "private:    {}",
        if i.is_private() { "yes" } else { "no" }
    )?;
    for (tier, urls) in m.trackers().iter().enumerate() {
        writeln!(out, "tracker:    [{tier}] {}", urls.join(" "))?;
    }
    writeln!(out, "files:")?;
    for (index, file) in i.files().iter().enumerate() {
        if file.is_padding() {
            continue;
        }
        let path = i.file_path(index).unwrap_or_default();
        writeln!(
            out,
            "  {:>10}  {}",
            format::bytes(file.length()),
            path.display()
        )?;
    }
    Ok(())
}

async fn download(
    input: Input,
    output: PathBuf,
    peers: Vec<SocketAddr>,
    config: SessionConfig,
    quiet: bool,
) -> Result<()> {
    let name = input.name();
    let session = Session::new(config).context("creating the session")?;
    let mut alerts = session.subscribe();
    let options = AddTorrentOptions::new(&output).with_peers(peers);
    let torrent = match input {
        Input::File(metainfo) => session.add_torrent(*metainfo, options),
        Input::Magnet(magnet) => session.add_magnet(&magnet, options),
    }
    .context("adding the torrent")?;
    let tty = std::io::stderr().is_terminal();
    let mut last_downloaded = 0u64;
    let mut ticks = 0u64;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let result = loop {
        let alert = tokio::select! {
            alert = alerts.recv() => alert,
            _ = &mut ctrl_c => break Err(anyhow::anyhow!("interrupted")),
        };
        let alert = match alert {
            Ok(a) if a.info_hash == torrent.info_hash() => a.alert,
            Ok(_) | Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => bail!("session closed unexpectedly"),
        };
        match alert {
            Alert::Stats(stats) => {
                if !quiet {
                    let rate = stats.downloaded.saturating_sub(last_downloaded);
                    last_downloaded = stats.downloaded;
                    ticks += 1;
                    let line = format::progress_line(&stats, rate);
                    let mut err = std::io::stderr().lock();
                    if tty {
                        let _ = write!(err, "\r{line}\x1b[K");
                    } else if ticks % 10 == 0 {
                        let _ = writeln!(err, "{line}");
                    }
                }
                // `Finished` can be dropped if a slow subscriber lags the
                // broadcast. The next stats snapshot still carries the state.
                if stats.status == TorrentStatus::Finished {
                    break Ok(());
                }
            }
            Alert::TrackerError { message } => {
                if tty {
                    let _ = writeln!(std::io::stderr());
                }
                let _ = writeln!(std::io::stderr(), "tracker: {message}");
            }
            Alert::Finished => break Ok(()),
            Alert::Error { message } => break Err(anyhow::anyhow!(message)),
            _ => {}
        }
    };
    if tty && !quiet {
        let _ = writeln!(std::io::stderr());
    }
    session.shutdown().await;
    result?;
    writeln!(
        std::io::stdout(),
        "{name}: complete in {}",
        output.display()
    )?;
    Ok(())
}
