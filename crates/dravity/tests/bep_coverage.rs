//! `docs/bep-coverage.md` has a row for every BEP the code cites.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn cited(text: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for (i, _) in text.match_indices("BEP") {
        let rest = text.get(i + 3..).unwrap_or_default();
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse() {
            out.insert(n);
        }
    }
    out
}

#[test]
fn every_cited_bep_has_a_row() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc = std::fs::read_to_string(root.join("docs/bep-coverage.md")).unwrap();
    let rows: BTreeSet<u32> = doc
        .lines()
        .filter_map(|l| l.strip_prefix("| "))
        .filter_map(|l| l.split(' ').next()?.parse().ok())
        .collect();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    let mut missing = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for n in cited(&text) {
            if !rows.contains(&n) {
                missing.insert(n);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "BEPs cited in code with no row in docs/bep-coverage.md: {missing:?}"
    );
    assert_eq!(cited("BEP 5, BEP52 and BEP x"), BTreeSet::from([5, 52]));
}
