//! Request handlers.

use std::convert::Infallible;
use std::io::SeekFrom;
use std::str::FromStr;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use dravity_core::{InfoHash, Magnet, Metainfo};
use dravity_engine::Priority;
use dravity_session::{AddTorrentOptions, SessionAlert, TorrentHandle};
use futures_util::Stream;
use tokio::sync::broadcast;

use crate::AppState;
use crate::dto::{
    self, AddMagnetDto, AddOptionsDto, AddQueryDto, AddedDto, AlertDto, EventsQueryDto, FileDto,
    FilePriorityDto, PeersDto, PrioritiesDto, RemoveQueryDto, SessionDto, TorrentDetailDto,
    TorrentDto,
};
use crate::error::ApiError;
use crate::{metrics, range};

/// Largest chunk read from a file stream at once.
const CHUNK: usize = 256 * 1024;

/// Rejects requests without the configured bearer token.
pub(crate) async fn auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(token) = state.config.token.as_deref() else {
        return next.run(request).await;
    };
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if given.is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes())) {
        return next.run(request).await;
    }
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(dto::ErrorDto {
            error: "missing or wrong bearer token".to_owned(),
        }),
    )
        .into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"dravity\""),
    );
    response
}

/// Compares without an early exit, so timing does not reveal how much of
/// the token matched (the length is not secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn info_hash(s: &str) -> Result<InfoHash, ApiError> {
    InfoHash::from_str(s).map_err(|e| ApiError::BadRequest(format!("bad info-hash: {e}")))
}

/// The torrent's running handle, or why there is none.
fn running(state: &AppState, info_hash: InfoHash) -> Result<TorrentHandle, ApiError> {
    if let Some(handle) = state.session.torrent(info_hash) {
        return Ok(handle);
    }
    if state
        .session
        .list()
        .iter()
        .any(|t| t.info_hash == info_hash)
    {
        Err(dravity_session::SessionError::NotRunning(info_hash).into())
    } else {
        Err(dravity_session::SessionError::Unknown(info_hash).into())
    }
}

pub(crate) async fn session_info(State(state): State<AppState>) -> Json<SessionDto> {
    let session = &state.session;
    Json(SessionDto {
        peer_id: hex(session.peer_id().as_bytes()),
        listen_port: session.listen_port(),
        dht: session.dht().is_some(),
        torrents: session.list().len(),
        port_mapping: session.port_mapping().as_ref().map(Into::into),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub(crate) async fn list(State(state): State<AppState>) -> Json<Vec<TorrentDto>> {
    let mut torrents = state.session.list();
    torrents.sort_by_key(|t| (t.added_at, t.info_hash));
    Json(torrents.iter().map(Into::into).collect())
}

/// Adds a torrent: a raw `.torrent` body (`application/x-bittorrent`, with
/// options in the query string) or a JSON magnet link.
pub(crate) async fn add(
    State(state): State<AppState>,
    Query(query): Query<AddQueryDto>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<AddedDto>), ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().starts_with("application/json"));
    let handle = if json {
        let request: AddMagnetDto = serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("bad JSON body: {e}")))?;
        let magnet = Magnet::parse(&request.magnet)
            .map_err(|e| ApiError::BadRequest(format!("bad magnet link: {e}")))?;
        let options = options(
            &state,
            AddOptionsDto {
                download_dir: request.download_dir,
                paused: request.paused,
                file_priorities: request.file_priorities,
                peers: request.peers,
            },
        );
        state.session.add_magnet(&magnet, options)?
    } else {
        let metainfo = Metainfo::from_bytes(&body)
            .map_err(|e| ApiError::BadRequest(format!("bad .torrent file: {e}")))?;
        let options = options(
            &state,
            AddOptionsDto {
                download_dir: query.download_dir,
                paused: query.paused,
                ..AddOptionsDto::default()
            },
        );
        state.session.add_torrent(metainfo, options)?
    };
    Ok((
        StatusCode::CREATED,
        Json(AddedDto {
            info_hash: handle.info_hash().to_hex(),
        }),
    ))
}

fn options(state: &AppState, dto: AddOptionsDto) -> AddTorrentOptions {
    let dir = dto
        .download_dir
        .unwrap_or_else(|| state.config.download_dir.clone());
    AddTorrentOptions::new(dir)
        .with_peers(dto.peers)
        .with_file_priorities(dto::unwrap_priorities(dto.file_priorities))
        .paused(dto.paused)
}

pub(crate) async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TorrentDetailDto>, ApiError> {
    let info_hash = info_hash(&id)?;
    let summary = state
        .session
        .list()
        .into_iter()
        .find(|t| t.info_hash == info_hash)
        .ok_or(dravity_session::SessionError::Unknown(info_hash))?;
    let progress = match state.session.torrent(info_hash) {
        Some(handle) => handle.progress().await.ok(),
        None => None,
    };
    let (stats, files) = match progress {
        Some(p) => (
            Some((&p.stats).into()),
            p.files.map(|files| files.iter().map(Into::into).collect()),
        ),
        None => (
            None,
            idle_files(&state, info_hash, &summary.file_priorities),
        ),
    };
    Ok(Json(TorrentDetailDto {
        torrent: (&summary).into(),
        stats,
        files,
    }))
}

/// Files of a torrent that is not running.
fn idle_files(
    state: &AppState,
    info_hash: InfoHash,
    priorities: &[Priority],
) -> Option<Vec<FileDto>> {
    let metainfo = state.session.metainfo(info_hash)?;
    let info = metainfo.info();
    let files = info
        .files()
        .iter()
        .enumerate()
        .map(|(index, f)| {
            FileDto::idle(
                index,
                info.file_path(index).unwrap_or_default(),
                f.length(),
                priorities.get(index).copied().unwrap_or_default(),
                f.is_padding(),
            )
        })
        .collect();
    Some(files)
}

pub(crate) async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<RemoveQueryDto>,
) -> Result<StatusCode, ApiError> {
    state
        .session
        .remove(info_hash(&id)?, query.delete_files)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.session.pause(info_hash(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.session.resume(info_hash(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_priorities(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PrioritiesDto>,
) -> Result<StatusCode, ApiError> {
    state
        .session
        .set_file_priorities(info_hash(&id)?, dto::unwrap_priorities(body.priorities))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_priority(
    State(state): State<AppState>,
    Path((id, index)): Path<(String, usize)>,
    Json(body): Json<FilePriorityDto>,
) -> Result<StatusCode, ApiError> {
    state
        .session
        .set_file_priority(info_hash(&id)?, index, body.priority.0)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn add_peers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PeersDto>,
) -> Result<StatusCode, ApiError> {
    running(&state, info_hash(&id)?)?
        .add_peers(body.peers)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Streams a file, honoring a single `Range`. Reads wait for missing
/// pieces, which are fetched first.
pub(crate) async fn content(
    State(state): State<AppState>,
    Path((id, index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let handle = running(&state, info_hash(&id)?)?;
    let mut stream = handle.open_file(index).await?;
    let len = stream.len();
    let requested = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| range::parse(v, len));
    let content_type = mime_for(&state, handle.info_hash(), index);
    let (status, start, count, content_range) = match requested {
        Some(Err(e)) => {
            let mut response = ApiError::BadRequest(e.to_string()).into_response();
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            insert(
                &mut response,
                header::CONTENT_RANGE,
                &format!("bytes */{len}"),
            );
            return Ok(response);
        }
        Some(Ok(Some(r))) => (
            StatusCode::PARTIAL_CONTENT,
            r.start,
            r.len(),
            Some(r.content_range(len)),
        ),
        Some(Ok(None)) | None => (StatusCode::OK, 0, len, None),
    };
    stream
        .seek(SeekFrom::Start(start))
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let body = futures_util::stream::unfold((stream, count), |(mut stream, left)| async move {
        if left == 0 {
            return None;
        }
        let max = usize::try_from(left).unwrap_or(CHUNK).min(CHUNK);
        match stream.read_chunk(max).await {
            Ok(chunk) if chunk.is_empty() => None,
            Ok(chunk) => {
                let read = u64::try_from(chunk.len()).unwrap_or(left);
                Some((Ok(chunk), (stream, left.saturating_sub(read))))
            }
            Err(e) => Some((Err(e), (stream, 0))),
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = status;
    insert(&mut response, header::CONTENT_TYPE, content_type);
    insert(&mut response, header::CONTENT_LENGTH, &count.to_string());
    insert(&mut response, header::ACCEPT_RANGES, "bytes");
    if let Some(value) = content_range {
        insert(&mut response, header::CONTENT_RANGE, &value);
    }
    Ok(response)
}

fn insert(response: &mut Response, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

/// A `Content-Type` from the file extension; players need it to start.
fn mime_for(state: &AppState, info_hash: InfoHash, index: usize) -> &'static str {
    let path = state
        .session
        .metainfo(info_hash)
        .and_then(|m| m.info().file_path(index));
    let ext = path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("ogg" | "oga") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("txt" | "nfo" | "srt") => "text/plain; charset=utf-8",
        Some("pdf") => "application/pdf",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

/// Session alerts as server-sent events, one JSON object each; `lagged`
/// events report alerts dropped because the client read too slowly.
pub(crate) async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventsQueryDto>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let only = query.info_hash.as_deref().map(info_hash).transpose()?;
    let rx = state.session.subscribe();
    let closing = state.closing.clone();
    let filter = move |a: &SessionAlert| {
        only.is_none_or(|ih| ih == a.info_hash)
            && (query.stats || !matches!(a.alert, dravity_engine::Alert::Stats(_)))
    };
    let stream = futures_util::stream::unfold((rx, filter), move |(mut rx, filter)| {
        let mut closing = closing.clone();
        async move {
            loop {
                let received = tokio::select! {
                    received = rx.recv() => received,
                    _ = closing.wait_for(|closing| *closing) => return None,
                };
                match received {
                    Ok(alert) if filter(&alert) => {
                        let dto = AlertDto::from(&alert);
                        let event = Event::default()
                            .event(dto.alert.name())
                            .json_data(&dto)
                            .unwrap_or_else(|_| Event::default().event("other"));
                        return Some((Ok(event), (rx, filter)));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let event = Event::default().event("lagged").data(n.to_string());
                        return Some((Ok(event), (rx, filter)));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub(crate) async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let samples = metrics::collect(&state.session).await;
    (
        [(header::CONTENT_TYPE, metrics::CONTENT_TYPE)],
        metrics::render(&samples),
    )
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, hex};

    #[test]
    fn tokens_compare_exactly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }
}
