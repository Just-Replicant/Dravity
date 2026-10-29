//! `dravity`: download and seed torrents from the command line.
//!
//! The CLI only sends session commands and renders session alerts; the
//! download loop lives in `dravity-session`.

mod format;
mod select;
mod serve;
mod tui;

use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use dravity_core::{Magnet, Metainfo};
use dravity_engine::{Alert, Priority, TorrentStatus};
use dravity_session::{AddTorrentOptions, ListenConfig, Session, SessionConfig};
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
    /// Download a torrent and exit when it is complete (or, with `--seed`,
    /// keep seeding until interrupted).
    Download {
        /// Path to a `.torrent` file, or a `magnet:` link.
        torrent: String,
        /// Download directory.
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// Extra peer to connect to (repeatable), e.g. `10.0.0.2:6881`.
        #[arg(long = "peer", value_name = "ADDR")]
        peers: Vec<SocketAddr>,
        /// Download only these files (indices from `dravity info`), e.g.
        /// `0,2-4`; the others are skipped.
        #[arg(long, value_name = "LIST", value_parser = select::parse_file_list)]
        files: Option<select::FileList>,
        /// Set one file's priority (repeatable): `INDEX=skip|low|normal|high`.
        #[arg(long = "file-priority", value_name = "INDEX=PRIORITY", value_parser = select::parse_file_priority)]
        file_priorities: Vec<(usize, Priority)>,
        /// Keep seeding after the download completes, until Ctrl-C.
        #[arg(long)]
        seed: bool,
        /// Directory for fastresume data (defaults to OUTPUT/.dravity).
        #[arg(long, value_name = "DIR", conflicts_with = "no_resume")]
        resume_dir: Option<PathBuf>,
        /// Neither read nor write fastresume data (always check existing files).
        #[arg(long)]
        no_resume: bool,
        /// Suppress the progress line.
        #[arg(short, long, conflicts_with = "tui")]
        quiet: bool,
        /// Show a full-screen terminal UI instead of the progress line.
        #[arg(long)]
        tui: bool,
        #[command(flatten)]
        network: NetworkArgs,
    },
    /// Run as a daemon: restore the saved torrents, seed them, take new ones
    /// from a watch folder and the HTTP API, until Ctrl-C.
    Serve {
        /// Where new torrents download to.
        #[arg(short, long, default_value = ".")]
        download_dir: PathBuf,
        /// Session state: the torrents, their settings and fastresume data
        /// (defaults to DOWNLOAD_DIR/.dravity).
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
        /// Add `.torrent` and `.magnet` files dropped into this directory.
        #[arg(long, value_name = "DIR")]
        watch: Option<PathBuf>,
        /// Address of the HTTP API.
        #[arg(
            long,
            value_name = "ADDR",
            default_value = "127.0.0.1:9091",
            conflicts_with = "no_http"
        )]
        http: SocketAddr,
        /// Do not serve the HTTP API.
        #[arg(long)]
        no_http: bool,
        /// Bearer token the HTTP API requires.
        #[arg(long, env = "DRAVITY_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Stop uploading once a torrent is complete.
        #[arg(long)]
        no_seed: bool,
        /// Show a full-screen terminal UI.
        #[arg(long)]
        tui: bool,
        #[command(flatten)]
        network: NetworkArgs,
    },
    /// Show what a `.torrent` file or `magnet:` link contains.
    Info {
        /// Path to a `.torrent` file, or a `magnet:` link.
        torrent: String,
    },
}

/// Networking options shared by `download` and `serve`.
#[derive(Debug, Args)]
struct NetworkArgs {
    /// TCP port to accept peers on (0: any free port). A port in use
    /// falls back to a free one.
    #[arg(long, default_value_t = 6881)]
    port: u16,
    /// Do not accept incoming connections; the port is only reported
    /// to trackers.
    #[arg(long)]
    no_listen: bool,
    /// Speak TCP only: no uTP (BEP 29) sockets.
    #[arg(long, conflicts_with = "prefer_utp")]
    no_utp: bool,
    /// Dial peers over uTP first, falling back to TCP.
    #[arg(long)]
    prefer_utp: bool,
    /// Do not ask the router (NAT-PMP, UPnP) to forward the port.
    #[arg(long)]
    no_port_mapping: bool,
    /// Upload rate limit in bytes per second, like `--download-limit`.
    #[arg(long, value_name = "RATE", value_parser = format::parse_rate)]
    upload_limit: Option<u64>,
    /// Peers unchoked (uploaded to) at once, plus one optimistic slot.
    #[arg(long, default_value_t = 4, value_name = "N")]
    upload_slots: usize,
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
}

impl NetworkArgs {
    /// A session configuration with these options.
    fn config(&self) -> SessionConfig {
        let mut config = SessionConfig::default();
        config.announce_port = self.port;
        config.listen = (!self.no_listen).then(|| {
            let mut listen = ListenConfig::new(self.port);
            if self.no_utp {
                listen.utp = None;
            }
            listen
        });
        config.prefer_utp = self.prefer_utp;
        if self.no_port_mapping {
            config.port_mapping = None;
        }
        config.engine.upload_slots = self.upload_slots;
        config.engine.upload_rate_limit = self.upload_limit.filter(|&r| r > 0);
        config.engine.max_peers = self.max_peers.max(1);
        config.engine.download_rate_limit = self.download_limit.filter(|&r| r > 0);
        if self.no_dht {
            config.dht = None;
        }
        config
    }
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

/// `--tui` draws on stdout and reads keys from stdin.
fn check_terminal(tui: bool) -> Result<()> {
    if tui && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!("--tui needs a terminal");
    }
    Ok(())
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
            files,
            file_priorities,
            seed,
            resume_dir,
            no_resume,
            quiet,
            tui,
            network,
        } => {
            check_terminal(tui)?;
            let input = load(&torrent)?;
            let priorities = match &input {
                Input::File(m) => select::file_priorities(
                    m.info().files().len(),
                    files.as_ref().map(|f| f.0.as_slice()),
                    &file_priorities,
                )
                .map_err(anyhow::Error::msg)?,
                Input::Magnet(_) if files.is_some() || !file_priorities.is_empty() => {
                    bail!(
                        "--files and --file-priority need a .torrent file (a magnet's files are unknown until its metadata arrives)"
                    )
                }
                Input::Magnet(_) => Vec::new(),
            };
            let mut config = network.config();
            config.engine.seed = seed;
            config.resume_dir = if no_resume {
                None
            } else {
                Some(resume_dir.unwrap_or_else(|| output.join(".dravity")))
            };
            let options = AddTorrentOptions::new(&output)
                .with_peers(peers)
                .with_file_priorities(priorities);
            let runtime = tokio::runtime::Runtime::new().context("starting the Tokio runtime")?;
            let ui = if tui {
                Ui::Tui
            } else if quiet {
                Ui::Quiet
            } else {
                Ui::Line
            };
            runtime.block_on(download(input, output, options, config, ui, seed))
        }
        Command::Serve {
            download_dir,
            state_dir,
            watch,
            http,
            no_http,
            token,
            no_seed,
            tui,
            network,
        } => {
            check_terminal(tui)?;
            let state_dir = state_dir.unwrap_or_else(|| download_dir.join(".dravity"));
            let mut config = network.config();
            config.engine.seed = !no_seed;
            config.state_dir = Some(state_dir.join("torrents"));
            config.resume_dir = Some(state_dir.join("resume"));
            let options = serve::Options {
                download_dir,
                watch,
                http: (!no_http).then_some(http),
                token,
                tui,
            };
            let runtime = tokio::runtime::Runtime::new().context("starting the Tokio runtime")?;
            runtime.block_on(serve::run(config, options))
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
    writeln!(
        out,
        "files:      (select with --files or --file-priority INDEX=…)"
    )?;
    for (index, file) in i.files().iter().enumerate() {
        if file.is_padding() {
            continue;
        }
        let path = i.file_path(index).unwrap_or_default();
        writeln!(
            out,
            "  [{index}] {:>10}  {}",
            format::bytes(file.length()),
            path.display()
        )?;
    }
    Ok(())
}

/// How `download` shows progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ui {
    /// A progress line on stderr.
    Line,
    /// Nothing but the final result.
    Quiet,
    /// The full-screen terminal UI.
    Tui,
}

async fn download(
    input: Input,
    output: PathBuf,
    options: AddTorrentOptions,
    config: SessionConfig,
    ui: Ui,
    seed: bool,
) -> Result<()> {
    let name = input.name();
    let session = Session::new(config).context("creating the session")?;
    if let (Some(port), Ui::Line) = (session.listen_port(), ui) {
        let _ = writeln!(std::io::stderr(), "accepting peers on port {port}");
    }
    let mut alerts = session.subscribe();
    let torrent = match input {
        Input::File(metainfo) => session.add_torrent(*metainfo, options),
        Input::Magnet(magnet) => session.add_magnet(&magnet, options),
    }
    .context("adding the torrent")?;
    if ui == Ui::Tui {
        let info_hash = torrent.info_hash();
        let mut finished = false;
        let ui_result = tui::run(&session, |view| {
            finished |= view.torrents.iter().any(|t| {
                t.info_hash == info_hash
                    && t.stats.is_some_and(|s| s.status == TorrentStatus::Finished)
            });
            finished && !seed
        })
        .await;
        session.shutdown().await;
        ui_result?;
        if !finished && !seed {
            bail!("interrupted");
        }
        if finished {
            writeln!(
                std::io::stdout(),
                "{name}: complete in {}",
                output.display()
            )?;
        }
        return Ok(());
    }
    let quiet = ui == Ui::Quiet;
    let tty = std::io::stderr().is_terminal();
    let mut last_downloaded = 0u64;
    let mut last_uploaded = 0u64;
    let mut ticks = 0u64;
    let mut complete = false;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let result = loop {
        let alert = tokio::select! {
            alert = alerts.recv() => alert,
            // Interrupting a seed is how it is meant to end.
            _ = &mut ctrl_c => break if complete {
                Ok(())
            } else {
                Err(anyhow::anyhow!("interrupted"))
            },
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
                    let up_rate = stats.uploaded.saturating_sub(last_uploaded);
                    last_downloaded = stats.downloaded;
                    last_uploaded = stats.uploaded;
                    ticks += 1;
                    let line = if stats.status == TorrentStatus::Finished {
                        format::seeding_line(&stats, up_rate)
                    } else {
                        format::progress_line(&stats, rate)
                    };
                    let mut err = std::io::stderr().lock();
                    if tty {
                        let _ = write!(err, "\r{line}\x1b[K");
                    } else if ticks % 10 == 0 {
                        let _ = writeln!(err, "{line}");
                    }
                }
                // `Finished` can be dropped if a slow subscriber lags the
                // broadcast. The next stats snapshot still carries the state.
                if stats.status == TorrentStatus::Finished && !complete {
                    complete = true;
                    if !seed {
                        break Ok(());
                    }
                    announce_seeding(&name, &output, tty, quiet);
                }
            }
            Alert::TrackerError { message } => {
                if tty {
                    let _ = writeln!(std::io::stderr());
                }
                let _ = writeln!(std::io::stderr(), "tracker: {message}");
            }
            Alert::UploadFailed { piece, message } => {
                if tty {
                    let _ = writeln!(std::io::stderr());
                }
                let _ = writeln!(std::io::stderr(), "upload: piece {piece}: {message}");
            }
            Alert::Finished if !complete => {
                complete = true;
                if !seed {
                    break Ok(());
                }
                announce_seeding(&name, &output, tty, quiet);
            }
            Alert::Error { message } => break Err(anyhow::anyhow!(message)),
            _ => {}
        }
    };
    if tty && !quiet {
        let _ = writeln!(std::io::stderr());
    }
    session.shutdown().await;
    result?;
    if !seed {
        writeln!(
            std::io::stdout(),
            "{name}: complete in {}",
            output.display()
        )?;
    }
    Ok(())
}

/// Reports that the download finished and seeding goes on.
fn announce_seeding(name: &str, output: &Path, tty: bool, quiet: bool) {
    let mut out = std::io::stdout().lock();
    if tty && !quiet {
        let _ = writeln!(std::io::stderr());
    }
    let _ = writeln!(
        out,
        "{name}: complete in {}; seeding (Ctrl-C to stop)",
        output.display()
    );
    let _ = out.flush();
}
