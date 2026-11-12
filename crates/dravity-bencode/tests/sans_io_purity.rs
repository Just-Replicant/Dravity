//! Purity tripwire: the sans-IO modules of this crate never do I/O, spawn
//! tasks or read the clock. The whole crate is sans-IO; besides its
//! dependency graph, this test keeps new files honest.

#![allow(clippy::indexing_slicing, clippy::panic)]
#![allow(clippy::expect_used)]

/// Tokens that mean a module does I/O, blocks, or reads the clock itself.
const FORBIDDEN: &[&str] = &[
    "tokio",
    ".await",
    "async fn",
    "async move",
    "UdpSocket",
    "TcpStream",
    "TcpListener",
    "ToSocketAddrs",
    "std::fs",
    "std::io::stdin",
    "std::thread",
    "Instant::now",
    "SystemTime::now",
    "reqwest",
    "rustix",
];

/// The forbidden tokens in `source`, ignoring comments (doc examples may
/// show a clock) and the unit tests at the end of the file.
fn violations(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut lines = source.lines().enumerate().peekable();
    while let Some((n, line)) = lines.next() {
        let code = line.trim_start();
        if code.starts_with("#[cfg(")
            && code.contains("test")
            && lines
                .peek()
                .is_some_and(|(_, next)| next.trim_start().starts_with("mod tests"))
        {
            break;
        }
        let code = code.split("//").next().unwrap_or_default();
        for token in FORBIDDEN {
            if code.contains(token) {
                found.push(format!("line {}: `{token}` in `{}`", n + 1, line.trim()));
            }
        }
    }
    found
}

#[test]
fn the_checker_sees_io() {
    assert!(violations("//! `Instant::now()` in docs is fine\nfn f() {}").is_empty());
    assert_eq!(
        violations("async fn read() { tokio::spawn(x).await; }").len(),
        3
    );
    assert!(
        violations("fn a() {}\n#[cfg(test)]\nmod tests { fn b() { std::thread::sleep(d); } }")
            .is_empty()
    );
}

#[test]
fn every_module_stays_pure() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .expect("src")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    assert!(!files.is_empty());
    let mut report = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(&path).expect("read");
        for v in violations(&source) {
            report.push(format!("{} {v}", path.display()));
        }
    }
    assert!(
        report.is_empty(),
        "I/O in a sans-IO crate:\n{}",
        report.join("\n")
    );
}
