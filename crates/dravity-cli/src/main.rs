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
use clap::{Args, Parser, Subcommand, ValueEnum};
use dravity_core::{Magnet, Metainfo};
use dravity_engine::{Alert, Priority, TorrentStatus};
use dravity_session::{
    AddTorrentOptions, EncryptionPolicy, ListenConfig, Session, SessionConfig, StorageBackend,
};
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
    /// Create a `.torrent` file from a file or directory.
    Create {
        /// The file or directory to share.
        path: PathBuf,
        /// Where to write the torrent (defaults to `<NAME>.torrent`).
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// `v1`, `v2` (BEP 52) or `hybrid` (both, one swarm).
        #[arg(
            long = "torrent-version",
            value_name = "VERSION",
            default_value = "hybrid"
        )]
        version: dravity_core::create::CreateVersion,
        /// Piece length, a power of two of at least 16K (default: chosen
        /// from the total size).
        #[arg(long, value_name = "SIZE", value_parser = format::parse_rate)]
        piece_length: Option<u64>,
        /// Tracker URL (repeatable). Each flag is its own tier; separate
        /// URLs with commas to put them in one tier.
        #[arg(short, long = "tracker", value_name = "URL")]
        trackers: Vec<String>,
        /// Web seed URL (repeatable, BEP 19).
        #[arg(short = 'w', long = "web-seed", value_name = "URL")]
        web_seeds: Vec<String>,
        /// Mark the torrent private (no DHT, PEX or local discovery).
        #[arg(long)]
        private: bool,
        /// Pad files to piece boundaries in a v1 torrent too (BEP 47).
        #[arg(long)]
        pad: bool,
        /// Free-form comment.
        #[arg(long)]
        comment: Option<String>,
        /// `source` tag, which gives the same files a distinct info hash
        /// per private tracker.
        #[arg(long)]
        source: Option<String>,
        /// Leave out the creation date (reproducible output).
        #[arg(long)]
        no_date: bool,
        /// Replace an existing output file.
        #[arg(short, long)]
        force: bool,
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
    /// Tune the upload slots every round from measured upload rates
    /// (`--upload-slots` is the starting value).
    #[arg(long)]
    auto_upload_slots: bool,
    /// How a seed picks peers to upload to: `fastest-upload`,
    /// `round-robin` (peers take turns) or `anti-leech` (peers just
    /// starting or nearly done first).
    #[arg(long, value_name = "MODE", default_value = "fastest-upload")]
    seed_choking: dravity_engine::SeedChoking,
    /// Maximum connected peers per torrent.
    #[arg(long, default_value_t = 50)]
    max_peers: usize,
    /// Maximum open peer connections across all torrents; 0 for no cap.
    #[arg(long, default_value_t = dravity_session::DEFAULT_MAX_CONNECTIONS, value_name = "N")]
    max_connections: usize,
    /// Memory for recently read torrent data, shared by all torrents (helps
    /// seeding); accepts `K`/`M`/`G` suffixes, 0 turns it off.
    #[arg(long, value_name = "SIZE", value_parser = format::parse_rate, default_value = "64M")]
    read_cache: u64,
    /// How new files reserve disk space: `none` grows them as data
    /// arrives, `sparse` sets the length at once, `full` reserves every
    /// block (fails early on a full disk).
    #[arg(long, value_name = "MODE", default_value = "none")]
    preallocate: dravity_session::PreallocateMode,
    /// Scrape trackers every this many minutes and show their counts
    /// (seeders, leechers, completed downloads); 0 never scrapes.
    #[arg(long, value_name = "MINUTES", default_value_t = 0)]
    scrape_interval: u64,
    /// When downloaded data is forced to disk: `never` (only on resume
    /// save and stop), `per-piece` (after each verified piece) or
    /// `on-complete`.
    #[arg(long, value_name = "WHEN", default_value = "never")]
    durability: dravity_session::Durability,
    /// Download rate limit in bytes per second; accepts `K`/`M`/`G`
    /// suffixes (binary), e.g. `500K` or `2M`.
    #[arg(long, value_name = "RATE", value_parser = format::parse_rate)]
    download_limit: Option<u64>,
    /// Download rate limit for all torrents together, in bytes per second
    /// (at least 16K); accepts `K`/`M`/`G` suffixes.
    #[arg(long, value_name = "RATE", value_parser = format::parse_rate)]
    global_download_limit: Option<u64>,
    /// Upload rate limit for all torrents together, in bytes per second
    /// (at least 16K); accepts `K`/`M`/`G` suffixes.
    #[arg(long, value_name = "RATE", value_parser = format::parse_rate)]
    global_upload_limit: Option<u64>,
    /// Run at most this many unfinished torrents at once; the rest wait
    /// queued and start, oldest first, as slots free up.
    #[arg(long, value_name = "N")]
    max_active_downloads: Option<usize>,
    /// Seed at most this many finished torrents at once; the rest wait
    /// queued.
    #[arg(long, value_name = "N")]
    max_active_seeds: Option<usize>,
    /// Super-seed complete torrents (BEP 16): show each peer one piece at
    /// a time so a first seed spreads the torrent faster.
    #[arg(long)]
    super_seed: bool,
    /// Do not exchange tracker lists with peers (`lt_tex`; private
    /// torrents never do).
    #[arg(long)]
    no_tracker_exchange: bool,
    /// Refuse peers in the ranges of this blocklist (eMule `ipfilter.dat`,
    /// PeerGuardian P2P text, or one address, range or CIDR per line).
    #[arg(long, value_name = "FILE")]
    ip_filter: Option<PathBuf>,
    /// Do not use the DHT for peer discovery.
    #[arg(long)]
    no_dht: bool,
    /// Do not look for peers on the local network (BEP 14 Local Service
    /// Discovery; private torrents never use it).
    #[arg(long)]
    no_lsd: bool,
    /// Never download from HTTP web seeds (BEP 19 `url-list`, magnet `ws`).
    #[arg(long)]
    no_web_seeds: bool,
    /// Let web seeds reach loopback, private (LAN) and link-local
    /// addresses. Off by default, so a torrent cannot make Dravity probe
    /// the local network.
    #[arg(long, conflicts_with = "no_web_seeds")]
    web_seeds_allow_private: bool,
    /// How torrent data is read and written. `io-uring` needs a build with
    /// the `uring` feature on Linux; without it, or where the kernel refuses
    /// io_uring, blocking I/O is used and a warning is logged.
    #[arg(long, value_enum, default_value_t = Backend::Blocking, value_name = "BACKEND")]
    storage_backend: Backend,
    /// Peer connection encryption (MSE/PE): `enabled` accepts both and dials
    /// encrypted first, falling back to plaintext; `required` only makes and
    /// accepts RC4-encrypted connections; `disabled` never encrypts.
    #[arg(long, value_enum, default_value_t = Encryption::Enabled, value_name = "POLICY")]
    encryption: Encryption,
}

/// `--encryption` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Encryption {
    /// Plain BitTorrent connections only.
    Disabled,
    /// Encrypt when the peer can; plaintext otherwise.
    Enabled,
    /// RC4-encrypted connections only.
    Required,
}

impl From<Encryption> for EncryptionPolicy {
    fn from(encryption: Encryption) -> Self {
        match encryption {
            Encryption::Disabled => Self::Disabled,
            Encryption::Enabled => Self::Enabled,
            Encryption::Required => Self::Required,
        }
    }
}

/// `--storage-backend` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// Positional reads and writes on a thread pool.
    Blocking,
    /// io_uring (`tokio-uring`) on a dedicated thread.
    IoUring,
}

impl From<Backend> for StorageBackend {
    fn from(backend: Backend) -> Self {
        match backend {
            Backend::Blocking => Self::Blocking,
            Backend::IoUring => Self::IoUring,
        }
    }
}

impl NetworkArgs {
    /// A session configuration with these options.
    fn config(&self) -> Result<SessionConfig> {
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
        config.engine.upload_slots_auto = self.auto_upload_slots;
        config.engine.seed_choking = self.seed_choking;
        config.engine.upload_rate_limit = self.upload_limit.filter(|&r| r > 0);
        config.engine.max_peers = self.max_peers.max(1);
        config.max_connections = (self.max_connections > 0).then_some(self.max_connections);
        config.read_cache = usize::try_from(self.read_cache).ok().filter(|&b| b > 0);
        config.preallocate = self.preallocate;
        config.scrape_interval = (self.scrape_interval > 0)
            .then(|| std::time::Duration::from_secs(self.scrape_interval.saturating_mul(60)));
        config.durability = self.durability;
        config.engine.download_rate_limit = self.download_limit.filter(|&r| r > 0);
        config.download_rate_limit = self.global_download_limit.filter(|&r| r > 0);
        config.upload_rate_limit = self.global_upload_limit.filter(|&r| r > 0);
        config.max_active_downloads = self.max_active_downloads;
        config.max_active_seeds = self.max_active_seeds;
        config.engine.super_seed = self.super_seed;
        if let Some(path) = &self.ip_filter {
            let filter = dravity_session::IpFilter::from_file(path)
                .with_context(|| format!("loading the IP filter {}", path.display()))?;
            if filter.skipped() > 0 {
                tracing::warn!(
                    skipped = filter.skipped(),
                    "IP filter lines that hold no range were skipped"
                );
            }
            config.ip_filter = Some(std::sync::Arc::new(filter));
        }
        config.engine.tracker_exchange = !self.no_tracker_exchange;
        if !self.no_lsd && config.listen.is_some() {
            config.lsd = Some(dravity_session::LsdConfig::default());
        }
        if self.no_dht {
            config.dht = None;
        }
        if self.no_web_seeds {
            config.web_seed = None;
        }
        if let Some(web_seed) = config.web_seed.as_mut() {
            web_seed.allow_private_addresses = self.web_seeds_allow_private;
        }
        config.storage_backend = self.storage_backend.into();
        config.encryption = self.encryption.into();
        Ok(config)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The UI owns the terminal. A log line on stderr is written onto that
    // screen and covers it, so `--tui` discards logs instead of printing them.
    init_tracing(tui_requested(&cli));
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Whether this command draws the full-screen UI.
fn tui_requested(cli: &Cli) -> bool {
    matches!(
        &cli.command,
        Command::Download { tui: true, .. } | Command::Serve { tui: true, .. }
    )
}

/// Installs the process logger. `tui` throws records away; anything else
/// goes to stderr at `warn`, unless `DRAVITY_LOG` says otherwise.
fn init_tracing(tui: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("DRAVITY_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    if tui {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::sink)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
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
        Command::Create {
            path,
            output,
            version,
            piece_length,
            trackers,
            web_seeds,
            private,
            pad,
            comment,
            source,
            no_date,
            force,
        } => {
            let mut options = dravity_core::create::CreateOptions::new(version);
            options.piece_length = piece_length
                .map(|l| u32::try_from(l).context("piece length is too large"))
                .transpose()?;
            options.trackers = trackers
                .iter()
                .map(|t| {
                    t.split(',')
                        .map(str::trim)
                        .filter(|u| !u.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .collect();
            options.web_seeds = web_seeds;
            options.private = private;
            options.pad_files = pad;
            options.comment = comment;
            options.source = source;
            options.created_by = Some(concat!("dravity/", env!("CARGO_PKG_VERSION")).to_owned());
            if !no_date {
                options.creation_date = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_secs()).ok());
            }
            create(&path, output, force, options)
        }
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
            let mut config = network.config()?;
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
            let mut config = network.config()?;
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

/// `dravity create`: hashes `path` and writes the torrent.
fn create(
    path: &Path,
    output: Option<PathBuf>,
    force: bool,
    options: dravity_core::create::CreateOptions,
) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{}", path.display()))?;
    let bytes = dravity_storage::create_torrent(&path, options, |_| {})
        .with_context(|| format!("creating a torrent of {}", path.display()))?;
    let m = Metainfo::from_bytes(&bytes).context("the created torrent does not parse")?;
    let output = output.unwrap_or_else(|| PathBuf::from(format!("{}.torrent", m.info().name())));
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    if force {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let mut file = open
        .open(&output)
        .with_context(|| format!("writing {}", output.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing {}", output.display()))?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "created:    {}", output.display())?;
    writeln!(out, "info-hash:  {}", m.info_hash())?;
    if let Some(v2) = m.info_hash_v2() {
        writeln!(out, "v2 hash:    {v2}")?;
    }
    writeln!(
        out,
        "pieces:     {} x {}",
        m.info().piece_count(),
        format::bytes(u64::from(m.info().piece_length()))
    )?;
    Ok(())
}

fn info(m: &Metainfo) -> Result<()> {
    let i = m.info();
    let mut out = std::io::stdout().lock();
    writeln!(out, "name:       {}", i.name())?;
    writeln!(out, "info-hash:  {}", m.info_hash())?;
    let version = match (i.version().has_v1(), i.version().has_v2()) {
        (true, true) => "hybrid (v1 + v2)",
        (false, true) => "v2",
        _ => "v1",
    };
    writeln!(out, "version:    {version}")?;
    if let Some(v2) = m.info_hash_v2() {
        writeln!(out, "v2 hash:    {v2}")?;
    }
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
    for url in m.web_seeds() {
        writeln!(out, "web seed:   {url}")?;
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
        let mut several = false;
        let ui_result = tui::run(&session, output.clone(), |view| {
            finished |= view
                .torrents
                .iter()
                .any(|t| t.info_hash == info_hash && t.is_finished());
            // One torrent: done when it finishes. Once another was added
            // from the screen: done when every torrent has.
            several |= view.torrents.len() > 1;
            let done = if several {
                !view.torrents.is_empty() && view.torrents.iter().all(tui::TorrentRow::is_finished)
            } else {
                finished
            };
            // A prompt or the key list has the keyboard: finishing must not
            // close the screen while that input is still open.
            done && !seed && !view.holds_input()
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
                    } else if ticks.is_multiple_of(10) {
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
            Alert::PieceLayersStalled {
                missing_layers,
                silent,
            } => {
                if tty {
                    let _ = writeln!(std::io::stderr());
                }
                let _ = writeln!(
                    std::io::stderr(),
                    "waiting for piece layers of {missing_layers} file(s): no peer has sent any for {}s (still asking)",
                    silent.as_secs()
                );
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
