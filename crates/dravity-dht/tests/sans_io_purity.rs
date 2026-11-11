//! Purity tripwire: the KRPC codec, BEP 42 ids and the datagram
//! classifier never do I/O, spawn tasks or read the clock.

#![allow(clippy::indexing_slicing, clippy::panic)]

const FORBIDDEN: &[&str] = &[
    "tokio",
    ".await",
    "async fn",
    "UdpSocket",
    "std::fs",
    "std::thread",
    "Instant::now",
    "SystemTime::now",
    "socket2",
];

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
    assert_eq!(violations("async fn f() { tokio::x().await }").len(), 3);
    assert!(violations("//! tokio in docs\nfn f() {}").is_empty());
}

#[test]
fn the_codecs_stay_pure() {
    for (name, source) in [
        ("demux.rs", include_str!("../src/demux.rs")),
        ("krpc.rs", include_str!("../src/krpc.rs")),
        ("secure_id.rs", include_str!("../src/secure_id.rs")),
    ] {
        let report = violations(source);
        assert!(report.is_empty(), "I/O in {name}:\n{}", report.join("\n"));
    }
}
