//! The session settings registry and the generated configuration
//! reference.
//!
//! [`SESSION_SETTINGS`] has one entry per [`SessionConfig`] field, and a
//! compile-time lock generated from the same list fails the build if the
//! struct and the registry drift. Together with
//! [`dravity_engine::schema::ENGINE_SETTINGS`] it drives
//! [`configuration_markdown`], which is checked against
//! `docs/configuration.md` by a test, classifies live settings (checked
//! against [`crate::LiveSettings`]) and lists the CLI flags (checked
//! against `dravity download --help`).

pub use dravity_engine::schema::{ENGINE_SETTINGS, Setting, SettingClass};

use crate::SessionConfig;

macro_rules! registry {
    ($( $field:ident : $class:ident, [$($flag:expr),*], $summary:expr; )*) => {
        /// Every [`SessionConfig`] field, in declaration order.
        pub const SESSION_SETTINGS: &[Setting] = &[$(
            Setting {
                name: stringify!($field),
                class: SettingClass::$class,
                cli: &[$($flag),*],
                summary: $summary,
            },
        )*];

        /// Compile-time lock: does not compile unless the registry names
        /// exactly the fields of [`SessionConfig`].
        #[allow(dead_code)]
        const fn completeness(c: &SessionConfig) {
            let SessionConfig { $( $field: _, )* } = c;
        }
    };
}

registry! {
peer_id_prefix: Restart, [], "Peer id prefix in Azureus style (`-DV0001-`); the rest is random.";
listen: Restart, ["--port", "--no-listen", "--no-utp"], "Accept incoming peer connections (TCP, and uTP unless `ListenConfig::utp` is `None`); `None` only dials out over TCP.";
prefer_utp: Restart, ["--prefer-utp"], "Dial peers over uTP first and fall back to TCP.";
port_mapping: Restart, ["--no-port-mapping"], "Ask the router (PCP, NAT-PMP, then UPnP IGD) to forward the listening port, TCP and (with uTP) UDP, so peers behind no NAT of their own can reach us, and open matching IPv6 firewall pinholes with PCP.";
announce_port: Restart, ["--port"], "Port reported to trackers when the session does not listen (for example behind a proxy that forwards to us).";
engine: Restart, [], "Per-torrent engine tunables.";
tracker: Restart, ["--allow-private-trackers"], "Tracker client settings, including `--allow-private-trackers` to opt in to private, loopback or link-local HTTP and UDP tracker origins (allowed HTTP paths must still end in an announce segment).";
num_want: Restart, [], "Peers requested per announce.";
connect_timeout: Restart, [], "Timeout for TCP connect plus handshake.";
tick_interval: Restart, [], "Engine timer period.";
peer_queue: Restart, [], "Messages queued towards one peer before it is dropped as too slow.";
event_queue: Restart, [], "Capacity of each torrent's event queue (peer, disk, hash, tracker).";
max_connections: Live, ["--max-connections"], "Open peer connections across every torrent, dialed or accepted, TCP or uTP (each torrent also keeps its own `max_peers`).";
hash_workers: Restart, [], "Concurrent piece-hash jobs across the whole session (piece checks, rechecks and smart-ban block digests of every torrent share them).";
hash_on_arrival: Restart, [], "Verify downloaded pieces from their blocks as they are written (incremental SHA-1, per-block SHA-256 leaves) instead of reading each finished piece back from storage.";
read_cache: Restart, ["--read-cache"], "Bytes of recently read torrent data kept in memory, shared by every torrent whose storage the session creates, so seeding a popular piece to many peers reads it from disk once (`dravity_storage::CachedStorage`).";
preallocate: Restart, ["--preallocate"], "How files the session creates reserve their space.";
durability: Restart, ["--durability"], "When written data is forced to disk.";
download_rate_limit: Live, ["--global-download-limit"], "Session-wide download limit in bytes per second, shared by every torrent on top of each torrent's own limit.";
upload_rate_limit: Live, ["--global-upload-limit"], "Session-wide upload limit in bytes per second.";
max_active_downloads: Live, ["--max-active-downloads"], "Torrents downloading at once; later ones wait in the queue.";
max_active_seeds: Live, ["--max-active-seeds"], "Torrents seeding at once; later ones wait in the queue.";
ip_filter: Restart, ["--ip-filter"], "Address ranges whose peers are refused and never dialed (`crate::IpFilter`); replace it at runtime with `crate::Session::set_ip_filter`.";
proxy: Restart, ["--proxy"], "Dial peers through this proxy (TCP only: uTP is skipped while a proxy is set).";
peer_dscp: Restart, ["--peer-dscp"], "DSCP value (0 to `crate::MAX_DSCP`) marked on peer TCP connections.";
geoip: Restart, ["--geoip"], "Country database for `crate::PeerStatus::country` (`crate::GeoIp`).";
network: Restart, [], "Carry peer connections over this `crate::transport::Network` instead of TCP and uTP sockets.";
lsd: Restart, ["--no-lsd"], "Local Service Discovery (BEP 14): find peers of the same torrents on the local network by multicast.";
scrape_interval: Restart, ["--scrape-interval"], "Scrape every tracker this often (BEP 48 / BEP 15) and show the counts in `crate::TrackerStatus::scrape`.";
stop_announce_timeout: Restart, [], "Time allowed for the `stopped` announce when a torrent stops.";
alert_buffer: Restart, [], "Broadcast buffer for alerts; slow subscribers miss older alerts.";
dht: Restart, ["--no-dht", "--native-dht", "--dht-ipv6"], "Run a DHT node (BEP 5) for peer discovery; `None` disables the DHT. `--dht-ipv6` starts a second native node on an IPv6 socket (BEP 32) and implies `--native-dht`.";
dht_lookup_timeout: Restart, [], "Time allowed for one DHT lookup.";
resume_dir: Restart, [], "Directory for fastresume files (`<info-hash>.resume`).";
state_dir: Restart, [], "Directory for session state (`crate::persist`): the torrents of the session, their directories, file priorities and paused flags, so `crate::Session::restore` can bring them back after a restart.";
web_seed: Restart, ["--no-web-seeds", "--web-seeds-allow-private"], "HTTP settings for web seeds (BEP 19: a torrent's `url-list`, a magnet link's `ws`).";
web_seed_retry: Restart, [], "Wait this long before trying a web seed again after it failed; doubles with every failure in a row, up to 32 times as long.";
storage_backend: Restart, ["--storage-backend"], "How file data is read and written for torrents whose storage the session creates (not for `AddTorrentOptions::with_storage`).";
encryption: Restart, ["--encryption"], "Message Stream Encryption (MSE/PE) for peer connections over TCP and uTP.";
max_piece_length: Restart, ["--max-piece-length"], "Largest torrent piece length accepted, in bytes, before torrent state is allocated (default: 32 MiB).";}

/// The configuration reference (`docs/configuration.md`), generated from
/// the registries.
#[must_use]
pub fn configuration_markdown() -> String {
    let mut out = String::from(
        "# Configuration reference\n\n\
         <!-- Generated from dravity_session::schema. Do not edit by hand: run\n\
         `DRAVITY_BLESS=1 cargo test -p dravity-session --lib schema` to refresh. -->\n\n\
         Every field of `SessionConfig` and of the per-torrent engine `Config`\n\
         (`SessionConfig::engine`). **live** settings change on a running session\n\
         through `Session::apply_settings` or `PATCH /api/v1/settings` (the field\n\
         name is the API name); **restart** settings are read when the session or\n\
         torrent starts.\n",
    );
    for (title, list) in [
        ("SessionConfig", SESSION_SETTINGS),
        ("Engine Config (`SessionConfig::engine`)", ENGINE_SETTINGS),
    ] {
        out.push_str(&format!(
            "\n## {title}\n\n| Setting | Applies | CLI | Description |\n|---|---|---|---|\n"
        ));
        for s in list {
            let cli = if s.cli.is_empty() {
                "—".to_owned()
            } else {
                s.cli
                    .iter()
                    .map(|f| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                s.name,
                s.class.as_str(),
                cli,
                s.summary.replace('|', "\\|")
            ));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn live_settings_match_the_registry() {
        // Exhaustive: a new live setting must be added here and classified.
        let crate::LiveSettings {
            download_rate_limit: _,
            upload_rate_limit: _,
            max_connections: _,
            max_active_downloads: _,
            max_active_seeds: _,
        } = crate::LiveSettings::default();
        let live: Vec<&str> = SESSION_SETTINGS
            .iter()
            .filter(|s| s.class == SettingClass::Live)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            live,
            [
                "max_connections",
                "download_rate_limit",
                "upload_rate_limit",
                "max_active_downloads",
                "max_active_seeds"
            ]
        );
        assert!(
            ENGINE_SETTINGS
                .iter()
                .all(|s| s.class == SettingClass::Restart)
        );
    }

    #[test]
    fn the_configuration_doc_is_current() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/configuration.md");
        let generated = configuration_markdown();
        if std::env::var_os("DRAVITY_BLESS").is_some() {
            std::fs::write(&path, &generated).unwrap();
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == generated,
            "docs/configuration.md is stale: run DRAVITY_BLESS=1 cargo test -p dravity-session --lib schema"
        );
    }
}
