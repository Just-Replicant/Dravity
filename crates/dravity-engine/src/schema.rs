//! The settings registry: one entry per [`Config`] field, with how it
//! takes effect, which CLI flags set it, and a one-line summary.
//!
//! The registry is the single source for `docs/configuration.md`, and a
//! compile-time lock (an exhaustive destructuring of [`Config`] generated
//! from the same list) fails the build when a field is added or removed
//! without updating it.

use crate::Config;

/// When a change to a setting takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingClass {
    /// Changeable on a running session (`Session::apply_settings`, the
    /// HTTP `PATCH /api/v1/settings`).
    Live,
    /// Read when the session (or, for engine settings, a torrent) starts.
    Restart,
}

impl SettingClass {
    /// Short name for docs and APIs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Restart => "restart",
        }
    }
}

/// One registered setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// Field name, which is also its API name.
    pub name: &'static str,
    /// When a change takes effect.
    pub class: SettingClass,
    /// CLI flags that set it (empty: library only).
    pub cli: &'static [&'static str],
    /// One-line summary (the first sentence of the field's docs).
    pub summary: &'static str,
}

macro_rules! registry {
    ($( $field:ident : $class:ident, [$($flag:expr),*], $summary:expr; )*) => {
        /// Every [`Config`] field, in declaration order.
        pub const ENGINE_SETTINGS: &[Setting] = &[$(
            Setting {
                name: stringify!($field),
                class: SettingClass::$class,
                cli: &[$($flag),*],
                summary: $summary,
            },
        )*];

        /// Compile-time lock: does not compile unless the registry names
        /// exactly the fields of [`Config`].
        #[allow(dead_code)]
        const fn completeness(c: &Config) {
            let Config { $( $field: _, )* } = c;
        }
    };
}

registry! {
max_peers: Restart, ["--max-peers"], "Maximum simultaneously connected (or connecting) peers.";
max_requests_per_peer: Restart, [], "Upper bound of a peer's AIMD request window (outstanding block requests).";
initial_requests_per_peer: Restart, [], "Request window a new peer starts with; it grows while blocks arrive in time and halves when a request times out.";
request_budget: Restart, [], "Block requests outstanding across all peers of the torrent.";
steal_after: Restart, [], "Request a block from a second peer when the first has not delivered it after this long and the second has nothing else to do (piece stealing from slow peers).";
endgame: Restart, [], "Once every remaining block is requested, request blocks from more than one peer and cancel the duplicates as copies arrive (endgame).";
download_rate_limit: Restart, ["--download-limit"], "Download rate limit in bytes per second (`None`: unlimited).";
pex: Restart, [], "Exchange peers with other peers (BEP 11).";
tracker_exchange: Restart, ["--no-tracker-exchange"], "Exchange tracker URLs with peers (`lt_tex`).";
holepunch: Restart, ["--no-holepunch"], "BEP 55 `ut_holepunch`: relay rendezvous between two connected peers, and ask a PEX source to relay one when dialing a peer it told us about fails.";
super_seed: Restart, ["--super-seed"], "BEP 16 super-seeding while the torrent is complete: peers see no bitfield, only one piece at a time chosen from the least shared, and get the next once they announce it.";
pex_interval: Restart, [], "Minimum time between two PEX messages to the same peer (BEP 11: one minute).";
dht: Restart, [], "Ask the driver to look up peers in the DHT (`Effect::DhtLookup`).";
dht_interval: Restart, [], "Time between DHT lookups for one torrent after a lookup finishes.";
dht_retry: Restart, [], "Time before another DHT lookup when the previous one timed out or the node was not ready.";
max_metadata_size: Restart, [], "Largest `info` dictionary accepted from peers for a magnet link.";
piece_layer_timeout: Restart, [], "How long a hybrid magnet waits for its v2 piece layers without any layer block arriving before the download starts on SHA-1 alone for the files still missing one (a v2-only magnet always waits: it has no other way to check its pieces).";
piece_layer_stall_alert: Restart, [], "A v2-only magnet whose piece layers have not progressed for this long raises `Alert::PieceLayersStalled` (and again after each further period of silence).";
request_timeout: Restart, [], "A request without an answer after this long is given to other peers.";
peer_timeout: Restart, [], "Drop a peer that sent nothing for this long.";
peer_retry: Restart, [], "Wait this long before connecting again to a peer that failed or dropped.";
keepalive_interval: Restart, [], "Send a keep-alive after this long without sending anything.";
default_announce_interval: Restart, [], "Re-announce interval when the tracker does not give one.";
min_announce_interval: Restart, [], "Lower bound applied to tracker-provided intervals.";
announce_retry: Restart, [], "Delay before retrying a failed announce.";
max_pending_writes: Restart, [], "Blocks handed to storage but not yet written; no new requests are sent while this many writes are pending (disk backpressure).";
check_existing: Restart, [], "Hash existing data on start to resume a partial download.";
check_concurrency: Restart, [], "Maximum concurrent hash jobs while checking.";
seed: Restart, [], "Keep the torrent in the swarm once it is complete: announce, accept connections and upload (seeding).";
upload_slots: Restart, ["--upload-slots"], "Peers unchoked for their upload to us (tit-for-tat) while downloading, or for the fastest uploads while seeding.";
optimistic_unchoke: Restart, [], "Give one extra slot to a choked peer in turn, so newcomers get data and better partners are discovered.";
seed_choking: Restart, ["--seed-choking"], "How regular slots are ranked while seeding (see `SeedChoking`).";
round_robin_quota: Restart, [], "Bytes a peer receives per turn under `SeedChoking::RoundRobin` before its slot passes on.";
upload_slots_auto: Restart, ["--auto-upload-slots"], "Tune `Self::upload_slots` every round from the measured upload rates: one more slot for each peer that takes more than an increasing threshold (1 KiB/s, 2 KiB/s, …), so slots grow while extra peers still add upload and stop when they would only split it.";
unchoke_interval: Restart, [], "Time between two choker rounds (BEP 3 suggests ten seconds).";
optimistic_unchoke_interval: Restart, [], "Time between two changes of the optimistic unchoke.";
snub_timeout: Restart, [], "A peer that holds our requests and sends no block for this long is snubbing us: it loses its regular slot (anti-snubbing) and can still receive the optimistic unchoke.";
upload_rate_limit: Restart, ["--upload-limit"], "Upload rate limit in bytes per second (`None`: unlimited).";
max_upload_queue: Restart, [], "Requests from one peer we queue before refusing more.";
max_pending_reads: Restart, [], "Block reads for uploads in flight at once across all peers (disk backpressure for seeding).";
fast_extension: Restart, [], "Advertise and use the BEP 6 fast extension (`have all`/`have none`, `reject`, `allowed fast`).";
allowed_fast: Restart, [], "Size of the BEP 6 allowed fast set offered to fast-extension peers (0: offer none).";
hash_fail_ban: Restart, [], "Ban a peer's IP address once it has this many strikes for pieces that failed the hash check (0: never ban).";
hash_fail_quiet: Restart, [], "Forget an address's hash-failure strikes after this long with no new failure, so a shared NAT is not banned for mistakes spread over the whole download.";
smart_ban: Restart, [], "Smart banning (default `true`): when a piece with several contributors fails, find out which block was bad instead of striking everyone.";
listen_port: Restart, [], "Port we accept connections on, advertised in the extension handshake (`p`) so peers that we dialed can tell others how to reach us.";}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_summaries_are_sentences() {
        let mut names: Vec<&str> = ENGINE_SETTINGS.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ENGINE_SETTINGS.len());
        for s in ENGINE_SETTINGS {
            assert!(s.summary.ends_with('.'), "{}", s.name);
            assert!(s.cli.iter().all(|f| f.starts_with("--")), "{}", s.name);
        }
    }
}
