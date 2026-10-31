//! Web seed URL and range planning over generated torrents.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::panic
)]

use std::borrow::Cow;

use dravity_bencode::{Dict, Value};
use dravity_core::Metainfo;
use dravity_webseed::{Segment, file_url, plan_range};
use proptest::prelude::*;

fn bstr(s: &str) -> Value<'static> {
    Value::Bytes(Cow::Owned(s.as_bytes().to_vec()))
}

fn dict(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
    Value::Dict(
        entries
            .into_iter()
            .map(|(k, v)| (Cow::Owned(k.as_bytes().to_vec()), v))
            .collect::<Dict<'_>>(),
    )
}

/// A multi-file torrent named `my dir` with `(path, length, padding)` files.
fn multi(files: &[(&[&str], u64, bool)]) -> Metainfo {
    let total: u64 = files.iter().map(|f| f.1).sum();
    let list = files
        .iter()
        .map(|(path, len, pad)| {
            let mut e = vec![
                ("length", Value::Int(*len as i64)),
                ("path", Value::List(path.iter().map(|c| bstr(c)).collect())),
            ];
            if *pad {
                e.push(("attr", bstr("p")));
            }
            dict(e)
        })
        .collect();
    let info = dict(vec![
        ("files", Value::List(list)),
        ("name", bstr("my dir")),
        ("piece length", Value::Int(16)),
        (
            "pieces",
            Value::Bytes(Cow::Owned(vec![0; total.div_ceil(16) as usize * 20])),
        ),
    ]);
    Metainfo::from_bytes(&dict(vec![("info", info)]).to_vec()).unwrap()
}

#[test]
fn multi_file_urls_live_below_the_torrent_name() {
    let m = multi(&[(&["a b", "c#.txt"], 10, false), (&[".pad", "6"], 6, true)]);
    let info = m.info();
    assert_eq!(
        file_url("http://h/seed", info, 0).unwrap(),
        "http://h/seed/my%20dir/a%20b/c%23.txt"
    );
    assert_eq!(
        file_url("https://h/seed/", info, 0).unwrap(),
        "https://h/seed/my%20dir/a%20b/c%23.txt"
    );
    assert_eq!(file_url("http://h/", info, 1), None, "padding");
    assert_eq!(file_url("http://h/", info, 2), None, "no such file");
}

#[test]
fn ranges_split_at_files_and_pad_with_zeros() {
    let m = multi(&[
        (&["a"], 10, false),
        (&[".pad", "6"], 6, true),
        (&["e"], 0, false),
        (&["b"], 20, false),
    ]);
    let plan = plan_range("http://h/", m.info(), 4..30).unwrap();
    assert_eq!(
        plan,
        [
            Segment::Fetch {
                url: "http://h/my%20dir/a".into(),
                offset: 4,
                length: 6,
                file_length: 10,
            },
            Segment::Zeros { length: 6 },
            Segment::Fetch {
                url: "http://h/my%20dir/b".into(),
                offset: 0,
                length: 14,
                file_length: 20,
            },
        ]
    );
    assert!(plan_range("http://h/", m.info(), 30..37).is_err());
    assert!(plan_range("http://h/", m.info(), 5..5).is_err());
}

proptest! {
    #[test]
    fn plans_cover_exactly_the_range(
        lengths in proptest::collection::vec(0u64..50, 1..8),
        a in 0u64..400,
        b in 0u64..400,
    ) {
        let names: Vec<String> = (0..lengths.len()).map(|i| format!("f{i}")).collect();
        let refs: Vec<[&str; 1]> = names.iter().map(|n| [n.as_str()]).collect();
        let specs: Vec<(&[&str], u64, bool)> = refs
            .iter()
            .zip(&lengths)
            .map(|(r, &l)| (&r[..], l, false))
            .collect();
        let total: u64 = lengths.iter().sum();
        prop_assume!(total > 0);
        let m = multi(&specs);
        let (start, end) = (a.min(b) % total, (a.max(b) % total) + 1);
        prop_assume!(start < end);
        let plan = plan_range("http://h/", m.info(), start..end).unwrap();
        prop_assert_eq!(plan.iter().map(Segment::len).sum::<u64>(), end - start);
        prop_assert!(plan.iter().all(|s| !s.is_empty()));
    }
}
