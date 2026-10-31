//! Web seeds (BEP 19) as peers.
//!
//! A web seed task looks like a peer task to the torrent owner: it reports
//! itself connected (the owner presents it as an incoming peer at a
//! synthetic address, see [`dravity_engine::web_seed_addr`], so it is never
//! dialed, retried or shared over PEX)
//! with every piece and unchoked, takes `request` and `cancel` commands, and
//! answers requests with `piece` messages. Contiguous requests are fetched
//! with one HTTP range (up to [`MAX_BATCH`] bytes) while further commands
//! keep arriving. Any HTTP failure ends the task; the owner tries the URL
//! again later. Data is verified like every other piece.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use dravity_core::{BlockRef, Metainfo};
use dravity_engine::PeerKey;
use dravity_webseed::{WebSeedClient, WebSeedError, plan_range};
use dravity_wire::{BlockRequest, Message};
use tokio::sync::mpsc;

use crate::peer::PeerCommand;
use crate::torrent::Input;

/// Most bytes fetched with one HTTP request.
pub(crate) const MAX_BATCH: u64 = 1024 * 1024;

/// What a web seed task serves.
#[derive(Debug, Clone)]
pub(crate) struct WebSeedParams {
    pub(crate) key: PeerKey,
    pub(crate) url: String,
    pub(crate) metainfo: Arc<Metainfo>,
    pub(crate) client: WebSeedClient,
}

type Fetch =
    Pin<Box<dyn Future<Output = (Vec<BlockRequest>, u64, Result<Vec<u8>, WebSeedError>)> + Send>>;

/// Runs one web seed until it fails or the owner drops its command queue.
pub(crate) async fn run(
    params: WebSeedParams,
    events: mpsc::Sender<Input>,
    mut commands: mpsc::Receiver<PeerCommand>,
) {
    let key = params.key;
    if !resolves(&params.url).await {
        tracing::debug!(url = %params.url, "web seed host did not resolve");
        let _ = events.send(Input::WebSeedFailed { key }).await;
        return;
    }
    let info = params.metainfo.info();
    let all = Message::Bitfield(full_bitfield(info.piece_count()));
    let hello = [
        Input::WebSeedReady { key },
        Input::Message { key, message: all },
        Input::Message {
            key,
            message: Message::Unchoke,
        },
    ];
    for input in hello {
        if events.send(input).await.is_err() {
            return;
        }
    }
    let mut queue: VecDeque<BlockRequest> = VecDeque::new();
    let mut inflight: Option<Fetch> = None;
    loop {
        if inflight.is_none()
            && let Some((batch, start, end)) = take_batch(&mut queue, &params.metainfo)
        {
            let plan = match plan_range(&params.url, params.metainfo.info(), start..end) {
                Ok(plan) => plan,
                Err(e) => {
                    tracing::debug!(url = %params.url, error = %e, "web seed plan failed");
                    break;
                }
            };
            let client = params.client.clone();
            inflight = Some(Box::pin(async move {
                let result = client.fetch(&plan).await;
                (batch, start, result)
            }));
        }
        tokio::select! {
            command = commands.recv() => match command {
                // The owner dropped us (disconnect, stop): no event needed.
                None => return,
                Some(PeerCommand::Send(Message::Request(r))) => queue.push_back(r),
                Some(PeerCommand::Send(Message::Cancel(r))) => queue.retain(|q| *q != r),
                // Choke, interest, keep-alives, haves: nothing to do.
                Some(PeerCommand::Send(_)) => {}
            },
            (batch, start, result) = async {
                match inflight.as_mut() {
                    Some(fetch) => fetch.await,
                    None => std::future::pending().await,
                }
            }, if inflight.is_some() => {
                inflight = None;
                let data = match result {
                    Ok(data) => Bytes::from(data),
                    Err(e) => {
                        tracing::debug!(url = %params.url, error = %e, "web seed request failed");
                        break;
                    }
                };
                if !deliver(&events, key, &params.metainfo, &batch, start, &data).await {
                    return;
                }
            }
        }
    }
    let _ = events.send(Input::Disconnected { key }).await;
}

/// Sends one `piece` message per block of `batch`, cut from `data` (which
/// starts at stream offset `start`). `false` when the owner is gone.
async fn deliver(
    events: &mpsc::Sender<Input>,
    key: PeerKey,
    metainfo: &Metainfo,
    batch: &[BlockRequest],
    start: u64,
    data: &Bytes,
) -> bool {
    let layout = metainfo.info().layout();
    for r in batch {
        let Some(at) = layout
            .piece_offset(r.piece)
            .map(|p| p + u64::from(r.offset) - start)
        else {
            continue;
        };
        let (Ok(from), Ok(len)) = (usize::try_from(at), usize::try_from(r.length)) else {
            continue;
        };
        if from + len > data.len() {
            continue;
        }
        let message = Message::Piece {
            piece: r.piece,
            offset: r.offset,
            data: data.slice(from..from + len),
        };
        if events.send(Input::Message { key, message }).await.is_err() {
            return false;
        }
    }
    true
}

/// Takes the first valid request and the requests right behind it that
/// continue it in the content stream, up to [`MAX_BATCH`] bytes. Returns
/// them with the stream range they cover.
fn take_batch(
    queue: &mut VecDeque<BlockRequest>,
    metainfo: &Metainfo,
) -> Option<(Vec<BlockRequest>, u64, u64)> {
    let layout = metainfo.info().layout();
    let range_of = |r: &BlockRequest| {
        let block = BlockRef {
            piece: r.piece,
            offset: r.offset,
            length: r.length,
        };
        layout
            .is_valid_request(block)
            .then(|| layout.piece_offset(r.piece))
            .flatten()
            .map(|p| {
                let s = p + u64::from(r.offset);
                (s, s + u64::from(r.length))
            })
    };
    let (first, start, mut end) = loop {
        let r = queue.pop_front()?;
        if let Some((s, e)) = range_of(&r) {
            break (r, s, e);
        }
    };
    let mut batch = vec![first];
    while let Some(next) = queue.front() {
        match range_of(next) {
            Some((s, e)) if s == end && e - start <= MAX_BATCH => {
                end = e;
                if let Some(r) = queue.pop_front() {
                    batch.push(r);
                }
            }
            _ => break,
        }
    }
    Some((batch, start, end))
}

/// A bitfield with all `pieces` set (spare bits clear).
fn full_bitfield(pieces: u32) -> Bytes {
    let pieces = usize::try_from(pieces).unwrap_or(0);
    let mut bits = vec![0xffu8; pieces.div_ceil(8)];
    let spare = bits.len() * 8 - pieces;
    if let Some(last) = bits.last_mut() {
        *last &= 0xffu8 << spare;
    }
    Bytes::from(bits)
}

/// Whether the web seed's host resolves at all (a cheap check before the
/// task reports itself; the HTTP client decides which addresses it may
/// actually use).
async fn resolves(url: &str) -> bool {
    let Some((host, port)) = dravity_webseed::plan::host_port(url) else {
        return false;
    };
    tokio::net::lookup_host((host.as_str(), port))
        .await
        .is_ok_and(|mut addrs| addrs.next().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitfields_clear_spare_bits() {
        assert_eq!(&full_bitfield(0)[..], b"");
        assert_eq!(&full_bitfield(8)[..], [0xff]);
        assert_eq!(&full_bitfield(10)[..], [0xff, 0xc0]);
    }
}
