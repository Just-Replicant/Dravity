//! `qbittorrent-nox` and `transmission-daemon`, each in its own profile
//! below a temporary directory, controlled over loopback HTTP.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::process::{Child, Command};

use super::http::{base64, form_escape, request};

/// The client's MSE setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Crypto {
    /// Plaintext only (libtorrent "disabled"; Transmission only
    /// "tolerates" MSE, prefers plaintext, and is paired with a Dravity
    /// that does not offer it).
    Plaintext,
    /// Accept both, prefer encrypted.
    Prefer,
    /// Encrypted connections only.
    Require,
}

/// Which transports the client uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transports {
    Both,
    TcpOnly,
    UtpOnly,
}

/// Progress of one torrent in a client.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Progress {
    pub(crate) done: bool,
    pub(crate) state: String,
}

/// Whether `name` is on `PATH`.
pub(crate) fn installed(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// A free loopback port (TCP and UDP), released for the client to bind.
pub(crate) fn free_port() -> u16 {
    loop {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

async fn wait_for<T>(
    what: &str,
    timeout: Duration,
    mut probe: impl AsyncFnMut() -> Option<T>,
) -> Result<T, String> {
    let start = Instant::now();
    loop {
        if let Some(v) = probe().await {
            return Ok(v);
        }
        if start.elapsed() > timeout {
            return Err(format!("timed out waiting for {what}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn spawn(program: &str, args: &[String], log: &Path) -> Child {
    let out = std::fs::File::create(log).unwrap();
    let err = out.try_clone().unwrap();
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("starting {program}: {e}"))
}

/// A running `qbittorrent-nox` (libtorrent): v1, v2 and hybrid.
pub(crate) struct Qbittorrent {
    _child: Child,
    api: SocketAddr,
    /// Its BitTorrent listen address.
    pub(crate) peer: SocketAddr,
    pub(crate) version: String,
}

impl Qbittorrent {
    pub(crate) async fn start(
        dir: &Path,
        crypto: Crypto,
        transports: Transports,
    ) -> Result<Self, String> {
        let peer_port = free_port();
        let api_port = free_port();
        let config = dir.join("qBittorrent/config");
        std::fs::create_dir_all(&config).unwrap();
        let save = dir.join("downloads");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::write(
            config.join("qBittorrent.conf"),
            format!(
                "[LegalNotice]\nAccepted=true\n\n[BitTorrent]\nSession\\Port={peer_port}\n\
                 Session\\DHTEnabled=false\nSession\\PeXEnabled=false\nSession\\LSDEnabled=false\n\
                 Session\\DefaultSavePath={}\n\n[Preferences]\nWebUI\\Port={api_port}\n\
                 WebUI\\Address=127.0.0.1\nWebUI\\LocalHostAuth=false\nWebUI\\CSRFProtection=false\n\
                 WebUI\\HostHeaderValidation=false\nConnection\\UPnP=false\n\
                 Connection\\ResolvePeerCountries=false\n",
                save.display()
            ),
        )
        .unwrap();
        let child = spawn(
            "qbittorrent-nox",
            &[
                format!("--profile={}", dir.display()),
                "--confirm-legal-notice".to_owned(),
            ],
            &dir.join("qbittorrent.log"),
        );
        let api = SocketAddr::from(([127, 0, 0, 1], api_port));
        let version = wait_for(
            "the qBittorrent Web API",
            Duration::from_secs(20),
            async || {
                let r = request(api, "GET", "/api/v2/app/version", &[], b"")
                    .await
                    .ok()?;
                (r.status == 200).then(|| r.text())
            },
        )
        .await?;
        let mut q = Self {
            _child: child,
            api,
            peer: SocketAddr::from(([127, 0, 0, 1], peer_port)),
            version,
        };
        // libtorrent: encryption 0 prefer, 1 require, 2 disabled; protocol 0
        // both, 2 uTP.
        let prefs = json!({
            "encryption": match crypto {
                Crypto::Prefer => 0,
                Crypto::Require => 1,
                Crypto::Plaintext => 2,
            },
            "bittorrent_protocol": match transports {
                Transports::Both => 0,
                Transports::TcpOnly => 1,
                Transports::UtpOnly => 2,
            },
            "dht": false, "pex": false, "lsd": false, "upnp": false,
            "max_ratio_enabled": false,
            // No GeoIP database download: the tests stay off the network.
            "resolve_peer_countries": false,
            "queueing_enabled": false,
        });
        q.post_form(
            "/api/v2/app/setPreferences",
            &format!("json={}", form_escape(&prefs.to_string())),
        )
        .await?;
        q.version = format!("qBittorrent {}", q.version.trim());
        Ok(q)
    }

    async fn post_form(&self, path: &str, body: &str) -> Result<String, String> {
        let r = request(
            self.api,
            "POST",
            path,
            &[("Content-Type", "application/x-www-form-urlencoded")],
            body.as_bytes(),
        )
        .await
        .map_err(|e| e.to_string())?;
        if r.status == 200 {
            Ok(r.text())
        } else {
            Err(format!("{path}: HTTP {}: {}", r.status, r.text()))
        }
    }

    /// Adds `torrent`, with its data in (or downloaded to) `save_path`.
    pub(crate) async fn add(&self, torrent: &[u8], save_path: &Path) -> Result<(), String> {
        let boundary = "dravity-interop-boundary";
        let mut body = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        };
        field("savepath", &save_path.display().to_string());
        field("stopped", "false");
        field("paused", "false");
        field("contentLayout", "Original");
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"t.torrent\"\r\n\
                 Content-Type: application/x-bittorrent\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(torrent);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let r = request(
            self.api,
            "POST",
            "/api/v2/torrents/add",
            &[("Content-Type", &content_type)],
            &body,
        )
        .await
        .map_err(|e| e.to_string())?;
        match (r.status, r.text().trim()) {
            (200, "Fails.") => Err("qBittorrent refused the torrent".to_owned()),
            (200, _) => Ok(()),
            (status, text) => Err(format!("torrents/add: HTTP {status}: {text}")),
        }
    }

    /// The torrent whose v1 or v2 info-hash (hex) is `hash`.
    async fn find(&self, hash: &str) -> Option<Value> {
        let r = request(self.api, "GET", "/api/v2/torrents/info", &[], b"")
            .await
            .ok()?;
        let list: Value = serde_json::from_slice(&r.body).ok()?;
        list.as_array()?
            .iter()
            .find(|t| {
                ["hash", "infohash_v1", "infohash_v2"]
                    .iter()
                    .any(|k| t[*k].as_str() == Some(hash))
            })
            .cloned()
    }

    pub(crate) async fn progress(&self, hash: &str) -> Option<Progress> {
        let t = self.find(hash).await?;
        let state = format!(
            "{} progress {} left {} downloaded {}",
            t["state"].as_str().unwrap_or(""),
            t["progress"],
            t["amount_left"],
            t["downloaded"]
        );
        let done = t["amount_left"].as_u64() == Some(0)
            && t["progress"].as_f64() == Some(1.0)
            && !state.starts_with("checking")
            && !state.starts_with("metaDL");
        Some(Progress { done, state })
    }

    /// Asks it to connect to `peer` now (it also learns peers from the
    /// tracker).
    pub(crate) async fn add_peer(&self, hash: &str, peer: SocketAddr) -> Result<(), String> {
        let id = self
            .find(hash)
            .await
            .and_then(|t| t["hash"].as_str().map(str::to_owned))
            .ok_or("torrent not found")?;
        self.post_form(
            "/api/v2/torrents/addPeers",
            &format!("hashes={id}&peers={}", form_escape(&peer.to_string())),
        )
        .await
        .map(drop)
    }
}

/// A running `transmission-daemon`.
pub(crate) struct Transmission {
    _child: Child,
    rpc: SocketAddr,
    session: std::sync::Mutex<String>,
    pub(crate) peer: SocketAddr,
    pub(crate) version: String,
}

impl Transmission {
    pub(crate) async fn start(
        dir: &Path,
        crypto: Crypto,
        transports: Transports,
    ) -> Result<Self, String> {
        let peer_port = free_port();
        let rpc_port = free_port();
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let save = dir.join("downloads");
        std::fs::create_dir_all(&save).unwrap();
        let settings = json!({
            "rpc-enabled": true, "rpc-port": rpc_port, "rpc-bind-address": "127.0.0.1",
            "rpc-authentication-required": false, "rpc-whitelist-enabled": false,
            "rpc-host-whitelist-enabled": false,
            "peer-port": peer_port, "peer-port-random-on-start": false,
            "port-forwarding-enabled": false, "dht-enabled": false, "lpd-enabled": false,
            "pex-enabled": false,
            "utp-enabled": transports != Transports::TcpOnly,
            "tcp-enabled": transports != Transports::UtpOnly,
            // 0 tolerate, 1 prefer, 2 require.
            "encryption": match crypto {
                Crypto::Plaintext => 0,
                Crypto::Prefer => 1,
                Crypto::Require => 2,
            },
            "download-dir": save, "incomplete-dir-enabled": false,
            "start-added-torrents": true, "ratio-limit-enabled": false,
            "download-queue-enabled": false, "seed-queue-enabled": false,
        });
        std::fs::write(config.join("settings.json"), settings.to_string()).unwrap();
        let child = spawn(
            "transmission-daemon",
            &[
                "-f".to_owned(),
                "-g".to_owned(),
                config.display().to_string(),
            ],
            &dir.join("transmission.log"),
        );
        let mut t = Self {
            _child: child,
            rpc: SocketAddr::from(([127, 0, 0, 1], rpc_port)),
            session: std::sync::Mutex::new(String::new()),
            peer: SocketAddr::from(([127, 0, 0, 1], peer_port)),
            version: String::new(),
        };
        let version = wait_for(
            "the Transmission RPC",
            Duration::from_secs(20),
            async || {
                let r = t
                    .call("session-get", json!({"fields": ["version"]}))
                    .await
                    .ok()?;
                r["arguments"]["version"].as_str().map(str::to_owned)
            },
        )
        .await?;
        t.version = format!("Transmission {version}");
        Ok(t)
    }

    async fn call(&self, method: &str, arguments: Value) -> Result<Value, String> {
        let body = json!({"method": method, "arguments": arguments}).to_string();
        for _ in 0..3 {
            let id = self.session.lock().unwrap().clone();
            let r = request(
                self.rpc,
                "POST",
                "/transmission/rpc",
                &[
                    ("X-Transmission-Session-Id", &id),
                    ("Content-Type", "application/json"),
                ],
                body.as_bytes(),
            )
            .await
            .map_err(|e| e.to_string())?;
            if r.status == 409 {
                // The session-id handshake (CSRF protection).
                *self.session.lock().unwrap() = r
                    .header("x-transmission-session-id")
                    .unwrap_or_default()
                    .to_owned();
                continue;
            }
            return serde_json::from_slice(&r.body).map_err(|e| format!("{e}: {}", r.text()));
        }
        Err("no Transmission session id".to_owned())
    }

    /// Adds `torrent`, with its data in (or downloaded to) `save_path`.
    pub(crate) async fn add(&self, torrent: &[u8], save_path: &Path) -> Result<(), String> {
        let r = self
            .call(
                "torrent-add",
                json!({"metainfo": base64(torrent), "download-dir": save_path, "paused": false}),
            )
            .await?;
        match r["result"].as_str() {
            Some("success") => Ok(()),
            other => Err(format!("torrent-add: {}", other.unwrap_or("no result"))),
        }
    }

    pub(crate) async fn progress(&self, hash: &str) -> Option<Progress> {
        let r = self
            .call(
                "torrent-get",
                json!({"fields": [
                    "hashString", "leftUntilDone", "percentDone", "status", "errorString",
                    "haveValid", "peersConnected",
                ]}),
            )
            .await
            .ok()?;
        let t = r["arguments"]["torrents"]
            .as_array()?
            .iter()
            .find(|t| t["hashString"].as_str() == Some(hash))?
            .clone();
        // Status: 0 stopped, 1-2 checking, 3-4 downloading, 5-6 seeding.
        let status = t["status"].as_u64().unwrap_or(0);
        let state = format!(
            "status {status} have {} peers {}{}",
            t["haveValid"],
            t["peersConnected"],
            t["errorString"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| format!(", error: {s}"))
                .unwrap_or_default()
        );
        let done = t["leftUntilDone"].as_u64() == Some(0)
            && t["percentDone"].as_f64() == Some(1.0)
            && status >= 4;
        Some(Progress { done, state })
    }
}

/// Either client.
pub(crate) enum Client {
    Qbittorrent(Qbittorrent),
    Transmission(Transmission),
}

impl Client {
    pub(crate) fn peer(&self) -> SocketAddr {
        match self {
            Self::Qbittorrent(c) => c.peer,
            Self::Transmission(c) => c.peer,
        }
    }

    pub(crate) fn version(&self) -> &str {
        match self {
            Self::Qbittorrent(c) => &c.version,
            Self::Transmission(c) => &c.version,
        }
    }

    pub(crate) async fn add(&self, torrent: &[u8], save_path: &Path) -> Result<(), String> {
        match self {
            Self::Qbittorrent(c) => c.add(torrent, save_path).await,
            Self::Transmission(c) => c.add(torrent, save_path).await,
        }
    }

    pub(crate) async fn progress(&self, hash: &str) -> Option<Progress> {
        match self {
            Self::Qbittorrent(c) => c.progress(hash).await,
            Self::Transmission(c) => c.progress(hash).await,
        }
    }

    /// Points the client at `peer` where it can be told directly
    /// (qBittorrent); returns whether it was. Transmission has no RPC for
    /// it and does not dial loopback peers a tracker returns, so the other
    /// side has to dial it.
    pub(crate) async fn add_peer(&self, hash: &str, peer: SocketAddr) -> bool {
        match self {
            Self::Qbittorrent(c) => c.add_peer(hash, peer).await.is_ok(),
            Self::Transmission(_) => false,
        }
    }

    /// Whether the client reads BEP 47 padding files from disk when it
    /// seeds a hybrid, instead of treating them as zeros (Transmission
    /// 4.1: without them its check never completes).
    pub(crate) const fn needs_pad_files(&self) -> bool {
        matches!(self, Self::Transmission(_))
    }

    /// Waits until the torrent `hash` is complete and checked.
    pub(crate) async fn wait_done(&self, hash: &str, timeout: Duration) -> Result<(), String> {
        let mut last = None;
        let start = Instant::now();
        loop {
            match self.progress(hash).await {
                Some(p) if p.done => {
                    if std::env::var_os("DRAVITY_INTEROP_DEBUG").is_some() {
                        eprintln!("{}: done: {}", self.version(), p.state);
                    }
                    return Ok(());
                }
                p => last = p.or(last),
            }
            if start.elapsed() > timeout {
                return Err(format!("{}: not complete: {last:?}", self.version()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
