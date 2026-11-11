//! IP filter lists: address ranges whose peers are refused.
//!
//! Reads the two common blocklist formats, eMule `ipfilter.dat`
//! (`001.002.004.000 - 001.002.004.255 , 000 , description`, where a
//! level of 127 or less blocks) and PeerGuardian P2P text
//! (`description:1.2.4.0-1.2.4.255`), plus bare addresses, `a - b`
//! ranges and CIDR blocks, IPv4 or IPv6. Ranges are merged into sorted,
//! disjoint intervals, so a lookup is a binary search.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

/// eMule levels at or below this block (eMule's and qBittorrent's default).
pub const EMULE_BLOCK_LEVEL: u32 = 127;

/// Why a filter list could not be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IpFilterError {
    /// The file could not be read.
    #[error("cannot read the IP filter: {0}")]
    Io(#[from] std::io::Error),
    /// No line held a range, but some lines could not be read.
    #[error("no usable range in the IP filter ({skipped} unreadable lines, first at line {first})")]
    Unreadable {
        /// Lines skipped.
        skipped: usize,
        /// First skipped line (1-based).
        first: usize,
    },
}

/// A set of blocked address ranges.
///
/// ```
/// use dravity_session::IpFilter;
///
/// let f = IpFilter::parse("Bad ISP:10.0.0.0-10.0.0.255\n192.168.5.0/24\n").unwrap();
/// assert!(f.is_blocked("10.0.0.7".parse().unwrap()));
/// assert!(f.is_blocked("192.168.5.200".parse().unwrap()));
/// assert!(!f.is_blocked("10.0.1.1".parse().unwrap()));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpFilter {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
    skipped: usize,
}

impl IpFilter {
    /// An empty filter (blocks nothing).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses a list. Comment (`#`, `//`) and blank lines are ignored; other
    /// lines that hold no range, and eMule lines above
    /// [`EMULE_BLOCK_LEVEL`], are skipped and counted in
    /// [`skipped`](Self::skipped).
    ///
    /// # Errors
    /// [`IpFilterError::Unreadable`] when no line gave a range but some
    /// could not be read (likely the wrong file).
    pub fn parse(text: &str) -> Result<Self, IpFilterError> {
        let mut filter = Self::new();
        let mut first = 0;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            match parse_line(line) {
                Some(Some((a, b))) => filter.add_range(a, b),
                // An allowed eMule level: not a range to block.
                Some(None) => {}
                None => {
                    if filter.skipped == 0 {
                        first = n + 1;
                    }
                    filter.skipped += 1;
                }
            }
        }
        filter.normalize();
        if filter.is_empty() && filter.skipped > 0 {
            return Err(IpFilterError::Unreadable {
                skipped: filter.skipped,
                first,
            });
        }
        Ok(filter)
    }

    /// Reads and parses a list file.
    ///
    /// # Errors
    /// As [`parse`](Self::parse), plus [`IpFilterError::Io`]. Bytes that
    /// are not UTF-8 are replaced, so a description in another encoding
    /// does not lose its line.
    pub fn from_file(path: &Path) -> Result<Self, IpFilterError> {
        let bytes = std::fs::read(path)?;
        Self::parse(&String::from_utf8_lossy(&bytes))
    }

    /// Blocks `start..=end` (swapped if reversed). Mixing an IPv4 and an
    /// IPv6 bound is ignored.
    pub fn add_range(&mut self, start: IpAddr, end: IpAddr) {
        match (canonical(start), canonical(end)) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let (a, b) = (u32::from(a), u32::from(b));
                self.v4.push((a.min(b), a.max(b)));
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let (a, b) = (u128::from(a), u128::from(b));
                self.v6.push((a.min(b), a.max(b)));
            }
            _ => return,
        }
        self.normalize();
    }

    /// Whether `ip` falls in a blocked range (IPv4-mapped IPv6 is checked
    /// as IPv4).
    #[must_use]
    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        match canonical(ip) {
            IpAddr::V4(v4) => contains(&self.v4, u32::from(v4)),
            IpAddr::V6(v6) => contains(&self.v6, u128::from(v6)),
        }
    }

    /// Disjoint ranges after merging.
    #[must_use]
    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    /// Whether nothing is blocked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines [`parse`](Self::parse) could not read.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    fn normalize(&mut self) {
        merge(&mut self.v4);
        merge(&mut self.v6);
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn merge<T: Copy + Ord + num_like::Step>(ranges: &mut Vec<(T, T)>) {
    ranges.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges.iter() {
        match out.last_mut() {
            Some(last) if a <= last.1.next_or_max() => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    *ranges = out;
}

fn contains<T: Copy + Ord>(ranges: &[(T, T)], x: T) -> bool {
    let i = ranges.partition_point(|&(a, _)| a <= x);
    i > 0 && ranges.get(i - 1).is_some_and(|&(_, b)| x <= b)
}

mod num_like {
    /// The next integer, saturating (for merging adjacent ranges).
    pub(super) trait Step {
        fn next_or_max(self) -> Self;
    }
    impl Step for u32 {
        fn next_or_max(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl Step for u128 {
        fn next_or_max(self) -> Self {
            self.saturating_add(1)
        }
    }
}

/// `Some(Some(range))` to block, `Some(None)` for an allowed eMule line,
/// `None` if unreadable.
fn parse_line(line: &str) -> Option<Option<(IpAddr, IpAddr)>> {
    // eMule: `a - b , level , description`.
    let fields: Vec<&str> = line.splitn(3, ',').collect();
    if fields.len() >= 2
        && let Some(range) = range(fields.first()?.trim())
    {
        let level: u32 = fields.get(1)?.trim().parse().ok()?;
        return Some((level <= EMULE_BLOCK_LEVEL).then_some(range));
    }
    if let Some(r) = range(line) {
        return Some(Some(r));
    }
    // P2P: `description:a-b` (the description may hold colons; IPv6
    // ranges are not used in this format).
    let (_, tail) = line.rsplit_once(':')?;
    range(tail.trim()).map(Some)
}

/// `a - b`, `a/len` or a single address.
fn range(s: &str) -> Option<(IpAddr, IpAddr)> {
    if let Some((a, b)) = s.split_once('-') {
        return Some((ip(a.trim())?, ip(b.trim())?));
    }
    if let Some((a, len)) = s.split_once('/') {
        let len: u32 = len.trim().parse().ok()?;
        return match ip(a.trim())? {
            IpAddr::V4(v4) if len <= 32 => {
                let mask = u32::MAX.checked_shl(32 - len).unwrap_or(0);
                let base = u32::from(v4) & mask;
                Some((
                    IpAddr::V4(Ipv4Addr::from(base)),
                    IpAddr::V4(Ipv4Addr::from(base | !mask)),
                ))
            }
            IpAddr::V6(v6) if len <= 128 => {
                let mask = u128::MAX.checked_shl(128 - len).unwrap_or(0);
                let base = u128::from(v6) & mask;
                Some((
                    IpAddr::V6(Ipv6Addr::from(base)),
                    IpAddr::V6(Ipv6Addr::from(base | !mask)),
                ))
            }
            _ => None,
        };
    }
    let one = ip(s)?;
    Some((one, one))
}

/// An address; IPv4 octets may have leading zeros (`001.002.003.004`, as
/// eMule writes them).
fn ip(s: &str) -> Option<IpAddr> {
    if s.contains(':') {
        return s.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    let mut octets = [0u8; 4];
    let mut parts = s.split('.');
    for o in &mut octets {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *o = part.parse().ok()?;
    }
    parts
        .next()
        .is_none()
        .then_some(IpAddr::V4(Ipv4Addr::from(octets)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn a(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn emule_dat() {
        let f = IpFilter::parse(
            "# comment\n\
             001.002.004.000 - 001.002.004.255 , 000 , Bad, Inc.\n\
             005.000.000.000 - 005.000.000.010 , 200 , allowed level\n\
             garbage line\n",
        )
        .unwrap();
        assert!(f.is_blocked(a("1.2.4.128")));
        assert!(!f.is_blocked(a("1.2.5.0")));
        assert!(!f.is_blocked(a("5.0.0.1")), "level 200 does not block");
        assert_eq!(f.skipped(), 1);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn p2p_cidr_ipv6_and_merging() {
        let f = IpFilter::parse(
            "Some: org:10.0.0.0-10.0.0.255\n\
             10.0.1.0-10.0.1.255\n\
             2001:db8::/32\n\
             192.0.2.7\n",
        )
        .unwrap();
        assert_eq!(f.len(), 3, "adjacent v4 ranges merge: {f:?}");
        assert!(f.is_blocked(a("10.0.1.9")));
        assert!(f.is_blocked(a("2001:db8:ffff::1")));
        assert!(!f.is_blocked(a("2001:db9::1")));
        assert!(f.is_blocked(a("192.0.2.7")));
        assert!(f.is_blocked(a("::ffff:10.0.0.3")), "mapped counts as v4");
        assert!(!f.is_blocked(a("192.0.2.8")));
    }

    #[test]
    fn bad_files_and_edges() {
        assert!(matches!(
            IpFilter::parse("not a list\nstill not\n"),
            Err(IpFilterError::Unreadable {
                skipped: 2,
                first: 1
            })
        ));
        assert!(IpFilter::parse("").unwrap().is_empty());
        for bad in ["1.2.3", "1.2.3.4.5", "1.2.3.256", "1.2.3.4/33", "+1.2.3.4"] {
            assert_eq!(range(bad), None, "{bad}");
        }
        let mut f = IpFilter::new();
        f.add_range(a("255.255.255.0"), a("255.255.255.255"));
        f.add_range(a("0.0.0.0"), a("0.0.0.0"));
        f.add_range(a("9.9.9.9"), a("1.1.1.1"));
        f.add_range(a("1.1.1.1"), a("::1"));
        assert!(f.is_blocked(a("255.255.255.255")) && f.is_blocked(a("0.0.0.0")));
        assert!(f.is_blocked(a("5.5.5.5")), "reversed bounds are swapped");
        assert_eq!(f.len(), 3);
        let all = IpFilter::parse("0.0.0.0/0").unwrap();
        assert!(all.is_blocked(a("8.8.8.8")));
    }
}
