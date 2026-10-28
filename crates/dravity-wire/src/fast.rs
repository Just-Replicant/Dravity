//! BEP 6 fast extension helpers.
//!
//! The message codecs (`suggest`, `have all`, `have none`, `reject`,
//! `allowed fast`) live in [`Message`](crate::Message). This module holds
//! the canonical *allowed fast set*: pieces a peer may download from us even
//! while choked, so a newcomer can get its first pieces quickly. The set
//! depends only on the peer's IPv4 address (its /24) and the info-hash, so
//! reconnecting from the same network does not yield a fresh set.

use std::net::Ipv4Addr;

use dravity_core::InfoHash;
use sha1::{Digest, Sha1};

/// Allowed fast set size libtorrent and most clients use.
pub const DEFAULT_ALLOWED_FAST: usize = 10;

/// The canonical allowed fast set (BEP 6) of `k` pieces for a peer at `ip`
/// in a torrent with `piece_count` pieces.
///
/// `k` is capped at `piece_count`. Indices come in generation order, which
/// is part of the BEP's test vector.
///
/// ```
/// use std::net::Ipv4Addr;
/// use dravity_core::InfoHash;
/// use dravity_wire::allowed_fast_set;
///
/// // The test vector from BEP 6.
/// let set = allowed_fast_set(&InfoHash::new([0xaa; 20]), Ipv4Addr::new(80, 4, 4, 200), 1313, 7);
/// assert_eq!(set, [1059, 431, 808, 1217, 287, 376, 1188]);
/// ```
#[must_use]
pub fn allowed_fast_set(
    info_hash: &InfoHash,
    ip: Ipv4Addr,
    piece_count: u32,
    k: usize,
) -> Vec<u32> {
    let k = k.min(usize::try_from(piece_count).unwrap_or(usize::MAX));
    let mut set = Vec::with_capacity(k);
    if k == 0 {
        return set;
    }
    let masked = u32::from(ip) & 0xFFFF_FF00;
    let mut x: Vec<u8> = masked.to_be_bytes().to_vec();
    x.extend_from_slice(info_hash.as_bytes());
    while set.len() < k {
        let digest: [u8; 20] = Sha1::digest(&x).into();
        for chunk in digest.chunks_exact(4) {
            if set.len() >= k {
                break;
            }
            let Ok(word) = <[u8; 4]>::try_from(chunk) else {
                continue;
            };
            let index = u32::from_be_bytes(word) % piece_count;
            if !set.contains(&index) {
                set.push(index);
            }
        }
        x = digest.to_vec();
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_bep_test_vectors() {
        let hash = InfoHash::new([0xaa; 20]);
        let ip = Ipv4Addr::new(80, 4, 4, 200);
        assert_eq!(
            allowed_fast_set(&hash, ip, 1313, 9),
            [1059, 431, 808, 1217, 287, 376, 1188, 353, 508]
        );
        // Only the /24 matters.
        assert_eq!(
            allowed_fast_set(&hash, Ipv4Addr::new(80, 4, 4, 1), 1313, 9),
            allowed_fast_set(&hash, ip, 1313, 9)
        );
    }

    #[test]
    fn small_torrents_terminate() {
        let hash = InfoHash::new([1; 20]);
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        assert!(allowed_fast_set(&hash, ip, 0, 10).is_empty());
        let mut all = allowed_fast_set(&hash, ip, 3, 10);
        all.sort_unstable();
        assert_eq!(all, [0, 1, 2]);
        assert_eq!(allowed_fast_set(&hash, ip, 1, 1), [0]);
    }
}
