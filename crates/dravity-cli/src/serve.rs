//! `dravity serve`: a long-running session with saved state, a watch
//! folder and the HTTP API.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use dravity_http::HttpConfig;
use dravity_session::{Session, SessionConfig, WatchConfig};

use crate::tui;

/// What to run besides the session.
#[derive(Debug)]
pub(crate) struct Options {
    /// Where new torrents download to.
    pub(crate) download_dir: PathBuf,
    /// Watch folder.
    pub(crate) watch: Option<PathBuf>,
    /// HTTP API address.
    pub(crate) http: Option<SocketAddr>,
    /// HTTP API bearer token.
    pub(crate) token: Option<String>,
    /// Full-screen UI instead of log lines.
    pub(crate) tui: bool,
}

/// Runs until Ctrl-C (or `q` in the UI).
pub(crate) async fn run(config: SessionConfig, options: Options) -> Result<()> {
    let session = Session::new(config).context("creating the session")?;
    let mut err = std::io::stderr();
    let report = session.restore().await.context("restoring the session")?;
    let _ = writeln!(err, "restored {} torrent(s)", report.restored.len());
    for (path, reason) in &report.failed {
        let _ = writeln!(err, "skipped {}: {reason}", path.display());
    }
    if let Some(port) = session.listen_port() {
        let _ = writeln!(err, "accepting peers on port {port}");
    }

    let watch = match &options.watch {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating the watch folder {}", dir.display()))?;
            let _ = writeln!(
                err,
                "watching {} for .torrent and .magnet files",
                dir.display()
            );
            Some(session.watch(WatchConfig::new(dir, &options.download_dir)))
        }
        None => None,
    };

    let (stop_http, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = match options.http {
        Some(addr) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding the HTTP API to {addr}"))?;
            let bound = listener.local_addr().unwrap_or(addr);
            let _ = writeln!(err, "HTTP API on http://{bound}/api/v1/");
            let mut config = HttpConfig::new(&options.download_dir).with_bind(bound);
            config.token.clone_from(&options.token);
            Some(tokio::spawn(dravity_http::serve(
                listener,
                session.clone(),
                config,
                async {
                    let _ = stopped.await;
                },
            )))
        }
        None => None,
    };

    let result = if options.tui {
        tui::run(&session, options.download_dir.clone(), |_| false).await
    } else {
        tokio::signal::ctrl_c().await.context("waiting for Ctrl-C")
    };

    let _ = stop_http.send(());
    if let Some(server) = server {
        match server.await {
            Ok(Err(e)) => {
                let _ = writeln!(std::io::stderr(), "HTTP API failed: {e}");
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "HTTP API task failed: {e}");
            }
            Ok(Ok(())) => {}
        }
    }
    if let Some(watch) = watch {
        watch.stop().await;
    }
    session.shutdown().await;
    result
}
