//! Country lookup for peer addresses from a CSV range database.
//!
//! Reads the common free "IP to country" CSV layout: one range per line,
//! `start,end,country`, where `start` and `end` are IP addresses (db-ip
//! lite) or decimal integers (IP2Location LITE), optionally quoted. Extra
//! columns are ignored. Ranges are merged into sorted tables and looked up
//! with a binary search.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

/// An ISO 3166-1 alpha-2 country code, upper case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CountryCode([u8; 2]);

impl CountryCode {
    /// Parses a two-letter code (any case); `None` for anything else,
    /// including the `-` and `ZZ` placeholders databases use for
    /// unassigned space.
    #[must_use]
    pub fn new(code: &str) -> Option<Self> {
        let [a, b] = <[u8; 2]>::try_from(code.as_bytes()).ok()?;
        if !a.is_ascii_alphabetic() || !b.is_ascii_alphabetic() {
            return None;
        }
        let code = [a.to_ascii_uppercase(), b.to_ascii_uppercase()];
        (code != *b"ZZ").then_some(Self(code))
    }

    /// The code as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("??")
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a GeoIP database could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum GeoIpError {
    /// The file could not be read.
    #[error("reading the GeoIP database: {0}")]
    Io(#[from] std::io::Error),
    /// No line held a usable range.
    #[error("the GeoIP database holds no usable range")]
    Empty,
}

/// Largest database file read.
pub const MAX_GEOIP_FILE: u64 = 256 * 1024 * 1024;

/// A loaded country database.
#[derive(Debug, Clone, Default)]
pub struct GeoIp {
    v4: Vec<(u32, u32, CountryCode)>,
    v6: Vec<(u128, u128, CountryCode)>,
    skipped: usize,
}

fn unquote(field: &str) -> &str {
    let field = field.trim();
    field
        .strip_prefix('"')
        .and_then(|f| f.strip_suffix('"'))
        .unwrap_or(field)
}

enum Bound {
    V4(u32),
    V6(u128),
    Int(u128),
}

fn bound(field: &str) -> Option<Bound> {
    let field = unquote(field);
    if let Ok(ip) = field.parse::<Ipv4Addr>() {
        Some(Bound::V4(u32::from(ip)))
    } else if let Ok(ip) = field.parse::<Ipv6Addr>() {
        Some(Bound::V6(u128::from(ip)))
    } else {
        field.parse::<u128>().ok().map(Bound::Int)
    }
}

fn merge<T: Ord + Copy>(ranges: &mut Vec<(T, T, CountryCode)>, next: impl Fn(T) -> Option<T>) {
    ranges.sort_unstable();
    let mut out: Vec<(T, T, CountryCode)> = Vec::with_capacity(ranges.len());
    for &(start, end, cc) in ranges.iter() {
        if let Some(last) = out.last_mut()
            && last.2 == cc
            && next(last.1).is_some_and(|n| n >= start)
        {
            last.1 = last.1.max(end);
            continue;
        }
        // Overlapping ranges of different countries: the earlier start
        // wins up to its end; the rest of the later one is kept.
        if let Some(last) = out.last()
            && start <= last.1
        {
            let Some(from) = next(last.1) else { continue };
            if from > end {
                continue;
            }
            out.push((from, end, cc));
            continue;
        }
        out.push((start, end, cc));
    }
    *ranges = out;
}

impl GeoIp {
    /// Parses a CSV database. Lines without a range and a country are
    /// counted in [`skipped`](Self::skipped).
    ///
    /// ```
    /// use dravity_session::GeoIp;
    ///
    /// let db = GeoIp::parse("1.0.0.0,1.0.0.255,AU\n\"16777472\",\"16778239\",\"CN\",\"China\"\n").unwrap();
    /// assert_eq!(db.lookup("1.0.0.7".parse().unwrap()).unwrap().as_str(), "AU");
    /// assert_eq!(db.lookup("1.0.1.1".parse().unwrap()).unwrap().as_str(), "CN");
    /// assert!(db.lookup("8.8.8.8".parse().unwrap()).is_none());
    /// ```
    ///
    /// # Errors
    /// [`GeoIpError::Empty`] if no line holds a range.
    pub fn parse(text: &str) -> Result<Self, GeoIpError> {
        let mut db = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !db.add_line(line) {
                db.skipped += 1;
            }
        }
        if db.v4.is_empty() && db.v6.is_empty() {
            return Err(GeoIpError::Empty);
        }
        merge(&mut db.v4, |x: u32| x.checked_add(1));
        merge(&mut db.v6, |x: u128| x.checked_add(1));
        Ok(db)
    }

    fn add_line(&mut self, line: &str) -> bool {
        let mut fields = line.split(',');
        let (Some(a), Some(b), Some(c)) = (fields.next(), fields.next(), fields.next()) else {
            return false;
        };
        let Some(cc) = CountryCode::new(unquote(c)) else {
            // Unassigned space (`-`, `ZZ`) is not an error.
            return matches!(unquote(c), "-" | "ZZ" | "zz");
        };
        match (bound(a), bound(b)) {
            (Some(Bound::V4(s)), Some(Bound::V4(e))) if s <= e => self.v4.push((s, e, cc)),
            (Some(Bound::V6(s)), Some(Bound::V6(e))) if s <= e => self.v6.push((s, e, cc)),
            (Some(Bound::Int(s)), Some(Bound::Int(e))) if s <= e => {
                // IP2Location: IPv4 space as integers; IPv6 files store
                // IPv4 as `::ffff:a.b.c.d` integers too.
                match (u32::try_from(s), u32::try_from(e)) {
                    (Ok(s), Ok(e)) => self.v4.push((s, e, cc)),
                    _ => self.v6.push((s, e, cc)),
                }
            }
            _ => return false,
        }
        true
    }

    /// Reads and parses a database file.
    ///
    /// # Errors
    /// [`GeoIpError`] if it cannot be read or holds no range.
    pub fn from_file(path: &Path) -> Result<Self, GeoIpError> {
        let bytes = crate::persist::read_bounded(path, MAX_GEOIP_FILE)?;
        Self::parse(&String::from_utf8_lossy(&bytes))
    }

    /// The country of `ip`, if the database covers it. IPv4-mapped IPv6
    /// addresses are looked up as IPv4.
    #[must_use]
    pub fn lookup(&self, ip: IpAddr) -> Option<CountryCode> {
        match ip {
            IpAddr::V4(v4) => find(&self.v4, u32::from(v4)),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => {
                    find(&self.v4, u32::from(v4)).or_else(|| find(&self.v6, u128::from(v6)))
                }
                None => find(&self.v6, u128::from(v6)),
            },
        }
    }

    /// Number of merged ranges.
    #[must_use]
    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    /// Whether the database is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines that held no usable range.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }
}

fn find<T: Ord + Copy>(ranges: &[(T, T, CountryCode)], x: T) -> Option<CountryCode> {
    let i = ranges.partition_point(|&(start, _, _)| start <= x);
    let &(_, end, cc) = ranges.get(i.checked_sub(1)?)?;
    (x <= end).then_some(cc)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cc(ip: &str, db: &GeoIp) -> Option<String> {
        db.lookup(ip.parse().unwrap()).map(|c| c.to_string())
    }

    #[test]
    fn formats_merging_and_lookup() {
        let db = GeoIp::parse(
            "# comment\n\
             1.0.0.0,1.0.0.255,au\n\
             1.0.1.0,1.0.1.255,AU,Australia\n\
             2.0.0.0,2.0.0.255,-\n\
             2001:db8::,2001:db8::ffff,DE\n\
             \"50331648\",\"50331903\",\"US\"\n\
             garbage\n\
             9.9.9.9,1.1.1.1,FR\n",
        )
        .unwrap();
        // The two AU ranges merged; garbage and the reversed range skipped.
        assert_eq!(db.len(), 3);
        assert_eq!(db.skipped(), 2);
        assert_eq!(cc("1.0.1.200", &db).as_deref(), Some("AU"));
        assert_eq!(cc("2.0.0.1", &db), None);
        assert_eq!(cc("3.0.0.5", &db).as_deref(), Some("US"));
        assert_eq!(cc("::ffff:3.0.0.5", &db).as_deref(), Some("US"));
        assert_eq!(cc("2001:db8::42", &db).as_deref(), Some("DE"));
        assert_eq!(cc("2001:db9::", &db), None);
        assert_eq!(cc("0.0.0.0", &db), None);
        assert_eq!(cc("255.255.255.255", &db), None);
        assert!(matches!(GeoIp::parse("x\n"), Err(GeoIpError::Empty)));
    }

    #[test]
    fn overlaps_keep_the_earlier_range() {
        let db = GeoIp::parse("10.0.0.0,10.0.0.100,AA\n10.0.0.50,10.0.0.200,BB\n").unwrap();
        assert_eq!(cc("10.0.0.75", &db).as_deref(), Some("AA"));
        assert_eq!(cc("10.0.0.150", &db).as_deref(), Some("BB"));
        assert!(CountryCode::new("ZZ").is_none());
        assert!(CountryCode::new("A1").is_none());
    }
}
