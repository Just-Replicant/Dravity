//! JSON bodies of the API. They are separate from the session types so the
//! wire format stays stable when the library changes.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use dravity_engine::{Alert, Priority, Stats, TorrentStatus};
use dravity_session::{
    FileProgress, PortMapMethod, PortMapProtocol, PortMapStatus, SessionAlert, TorrentSummary,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A file priority on the wire: `"skip"`, `"low"`, `"normal"` or `"high"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityDto(pub Priority);

impl Serialize for PriorityDto {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for PriorityDto {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map(Self).map_err(serde::de::Error::custom)
    }
}

fn priorities(priorities: &[Priority]) -> Vec<PriorityDto> {
    priorities.iter().copied().map(PriorityDto).collect()
}

pub(crate) fn unwrap_priorities(priorities: Vec<PriorityDto>) -> Vec<Priority> {
    priorities.into_iter().map(|p| p.0).collect()
}

/// `GET /api/v1/session`.
#[derive(Debug, Serialize)]
pub(crate) struct SessionDto {
    pub(crate) peer_id: String,
    pub(crate) listen_port: Option<u16>,
    pub(crate) dht: bool,
    pub(crate) torrents: usize,
    pub(crate) port_mapping: Option<PortMappingDto>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PortMappingDto {
    method: Option<&'static str>,
    external_ip: Option<IpAddr>,
    mapped: Vec<MappedDto>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct MappedDto {
    protocol: &'static str,
    port: u16,
    external_port: u16,
}

impl From<&PortMapStatus> for PortMappingDto {
    fn from(status: &PortMapStatus) -> Self {
        Self {
            method: status.method.map(|m| match m {
                PortMapMethod::NatPmp => "nat-pmp",
                PortMapMethod::Upnp => "upnp",
            }),
            external_ip: status.external_ip.map(IpAddr::V4),
            mapped: status
                .mapped
                .iter()
                .map(|m| MappedDto {
                    protocol: match m.mapping.protocol {
                        PortMapProtocol::Tcp => "tcp",
                        PortMapProtocol::Udp => "udp",
                    },
                    port: m.mapping.port,
                    external_port: m.external_port,
                })
                .collect(),
            error: status.error.clone(),
        }
    }
}

/// One torrent in `GET /api/v1/torrents`.
#[derive(Debug, Serialize)]
pub(crate) struct TorrentDto {
    info_hash: String,
    name: Option<String>,
    download_dir: PathBuf,
    state: &'static str,
    has_metadata: bool,
    file_priorities: Vec<PriorityDto>,
    added_at: u64,
}

impl From<&TorrentSummary> for TorrentDto {
    fn from(t: &TorrentSummary) -> Self {
        Self {
            info_hash: t.info_hash.to_hex(),
            name: t.name.clone(),
            download_dir: t.download_dir.clone(),
            state: state(t),
            has_metadata: t.has_metadata,
            file_priorities: priorities(&t.file_priorities),
            added_at: t.added_at,
        }
    }
}

/// `running`, `paused` or `stopped` (after an error).
pub(crate) const fn state(t: &TorrentSummary) -> &'static str {
    if t.running {
        "running"
    } else if t.paused {
        "paused"
    } else {
        "stopped"
    }
}

/// `GET /api/v1/torrents/{info_hash}`.
#[derive(Debug, Serialize)]
pub(crate) struct TorrentDetailDto {
    #[serde(flatten)]
    pub(crate) torrent: TorrentDto,
    /// Live numbers, while running.
    pub(crate) stats: Option<StatsDto>,
    /// Files, once the metadata is known.
    pub(crate) files: Option<Vec<FileDto>>,
}

/// Engine progress numbers.
#[derive(Debug, Serialize)]
pub(crate) struct StatsDto {
    status: &'static str,
    piece_count: u32,
    pieces_have: u32,
    total_length: u64,
    bytes_have: u64,
    wanted_left: u64,
    downloaded: u64,
    uploaded: u64,
    peers: usize,
    connecting: usize,
    known_peers: usize,
    unchoked: usize,
    interested: usize,
    banned: usize,
}

impl From<&Stats> for StatsDto {
    fn from(s: &Stats) -> Self {
        Self {
            status: status(s.status),
            piece_count: s.piece_count,
            pieces_have: s.pieces_have,
            total_length: s.total_length,
            bytes_have: s.bytes_have,
            wanted_left: s.wanted_left,
            downloaded: s.downloaded,
            uploaded: s.uploaded,
            peers: s.peers,
            connecting: s.connecting,
            known_peers: s.known_peers,
            unchoked: s.unchoked,
            interested: s.interested,
            banned: s.banned,
        }
    }
}

/// Wire name of an engine state.
pub(crate) const fn status(status: TorrentStatus) -> &'static str {
    match status {
        TorrentStatus::Idle => "idle",
        TorrentStatus::FetchingMetadata => "fetching_metadata",
        TorrentStatus::Checking => "checking",
        TorrentStatus::Downloading => "downloading",
        TorrentStatus::Finished => "finished",
        TorrentStatus::Stopped => "stopped",
        _ => "unknown",
    }
}

/// One file of a torrent.
#[derive(Debug, Serialize)]
pub(crate) struct FileDto {
    index: usize,
    path: PathBuf,
    length: u64,
    /// Bytes in verified pieces; `None` when the torrent is not running.
    have: Option<u64>,
    priority: PriorityDto,
    padding: bool,
}

impl From<&FileProgress> for FileDto {
    fn from(f: &FileProgress) -> Self {
        Self {
            index: f.index,
            path: f.path.clone(),
            length: f.length,
            have: Some(f.have),
            priority: PriorityDto(f.priority),
            padding: f.padding,
        }
    }
}

impl FileDto {
    /// A file of a torrent that is not running (progress unknown).
    pub(crate) const fn idle(
        index: usize,
        path: PathBuf,
        length: u64,
        priority: Priority,
        padding: bool,
    ) -> Self {
        Self {
            index,
            path,
            length,
            have: None,
            priority: PriorityDto(priority),
            padding,
        }
    }
}

/// `POST /api/v1/torrents` with a JSON body (a magnet link).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddMagnetDto {
    pub(crate) magnet: String,
    pub(crate) download_dir: Option<PathBuf>,
    #[serde(default)]
    pub(crate) paused: bool,
    #[serde(default)]
    pub(crate) file_priorities: Vec<PriorityDto>,
    #[serde(default)]
    pub(crate) peers: Vec<SocketAddr>,
}

/// Options of an added torrent.
#[derive(Debug, Default)]
pub(crate) struct AddOptionsDto {
    pub(crate) download_dir: Option<PathBuf>,
    pub(crate) paused: bool,
    pub(crate) file_priorities: Vec<PriorityDto>,
    pub(crate) peers: Vec<SocketAddr>,
}

/// Query string of a raw `.torrent` upload (no lists in query strings).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddQueryDto {
    pub(crate) download_dir: Option<PathBuf>,
    #[serde(default)]
    pub(crate) paused: bool,
}

/// Answer to an add.
#[derive(Debug, Serialize)]
pub(crate) struct AddedDto {
    pub(crate) info_hash: String,
}

/// `PUT /api/v1/torrents/{info_hash}/files`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrioritiesDto {
    pub(crate) priorities: Vec<PriorityDto>,
}

/// `PUT /api/v1/torrents/{info_hash}/files/{index}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilePriorityDto {
    pub(crate) priority: PriorityDto,
}

/// `POST /api/v1/torrents/{info_hash}/peers`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeersDto {
    pub(crate) peers: Vec<SocketAddr>,
}

/// `DELETE /api/v1/torrents/{info_hash}` query.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveQueryDto {
    #[serde(default)]
    pub(crate) delete_files: bool,
}

/// `GET /api/v1/events` query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventsQueryDto {
    /// Include the periodic `stats` alerts (default `true`).
    #[serde(default = "yes")]
    pub(crate) stats: bool,
    /// Only alerts of this torrent.
    pub(crate) info_hash: Option<String>,
}

const fn yes() -> bool {
    true
}

/// An alert on the event stream.
#[derive(Debug, Serialize)]
pub(crate) struct AlertDto {
    pub(crate) info_hash: String,
    #[serde(flatten)]
    pub(crate) alert: AlertKind,
}

/// The alert itself, tagged by `type`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AlertKind {
    StateChanged {
        status: &'static str,
    },
    Stats(StatsDto),
    PieceCompleted {
        piece: u32,
    },
    HashFailed {
        piece: u32,
    },
    TrackerError {
        message: String,
    },
    UploadFailed {
        piece: u32,
        message: String,
    },
    Finished,
    MetadataReceived,
    PeerBanned {
        ip: IpAddr,
    },
    Error {
        message: String,
    },
    /// An alert this API version does not know.
    Other,
}

impl AlertKind {
    /// The SSE event name.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::StateChanged { .. } => "state_changed",
            Self::Stats(_) => "stats",
            Self::PieceCompleted { .. } => "piece_completed",
            Self::HashFailed { .. } => "hash_failed",
            Self::TrackerError { .. } => "tracker_error",
            Self::UploadFailed { .. } => "upload_failed",
            Self::Finished => "finished",
            Self::MetadataReceived => "metadata_received",
            Self::PeerBanned { .. } => "peer_banned",
            Self::Error { .. } => "error",
            Self::Other => "other",
        }
    }
}

impl From<&SessionAlert> for AlertDto {
    fn from(a: &SessionAlert) -> Self {
        let alert = match &a.alert {
            Alert::StateChanged(s) => AlertKind::StateChanged { status: status(*s) },
            Alert::Stats(s) => AlertKind::Stats(s.into()),
            Alert::PieceCompleted { piece } => AlertKind::PieceCompleted { piece: *piece },
            Alert::HashFailed { piece } => AlertKind::HashFailed { piece: *piece },
            Alert::TrackerError { message } => AlertKind::TrackerError {
                message: message.clone(),
            },
            Alert::UploadFailed { piece, message } => AlertKind::UploadFailed {
                piece: *piece,
                message: message.clone(),
            },
            Alert::Finished => AlertKind::Finished,
            Alert::MetadataReceived => AlertKind::MetadataReceived,
            Alert::PeerBanned { ip } => AlertKind::PeerBanned { ip: *ip },
            Alert::Error { message } => AlertKind::Error {
                message: message.clone(),
            },
            _ => AlertKind::Other,
        };
        Self {
            info_hash: a.info_hash.to_hex(),
            alert,
        }
    }
}

/// An error body: `{"error": "..."}`.
#[derive(Debug, Serialize)]
pub(crate) struct ErrorDto {
    pub(crate) error: String,
}

#[cfg(test)]
mod tests {
    use dravity_engine::Priority;

    use super::PriorityDto;

    #[test]
    fn priorities_are_lowercase_names() {
        let json =
            serde_json::to_string(&[PriorityDto(Priority::Skip), PriorityDto(Priority::High)]);
        assert_eq!(json.ok().as_deref(), Some(r#"["skip","high"]"#));
        let back: Result<Vec<PriorityDto>, _> = serde_json::from_str(r#"["low","normal"]"#);
        assert_eq!(
            back.ok(),
            Some(vec![
                PriorityDto(Priority::Low),
                PriorityDto(Priority::Normal)
            ])
        );
        assert!(serde_json::from_str::<PriorityDto>(r#""urgent""#).is_err());
    }
}
