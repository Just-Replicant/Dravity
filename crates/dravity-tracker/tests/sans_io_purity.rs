//! Purity tripwire: the sans-IO modules of this crate never do I/O, spawn
//! tasks or read the clock. They share the crate with I/O code, so the
//! dependency graph cannot enforce it; this test does.

#![allow(clippy::indexing_slicing, clippy::panic)]

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

/// The sans-IO modules: protocol state machines and codecs.
const MODULES: &[(&str, &str)] = &[
    ("http.rs", include_str!("../src/http.rs")),
    ("udp.rs", include_str!("../src/udp.rs")),
    ("guard.rs", include_str!("../src/guard.rs")),
    ("types.rs", include_str!("../src/types.rs")),
];

#[test]
fn sans_io_modules_stay_pure() {
    let mut report = Vec::new();
    for (name, source) in MODULES {
        for v in violations(source) {
            report.push(format!("src/{name} {v}"));
        }
    }
    assert!(
        report.is_empty(),
        "I/O in a sans-IO module:\n{}",
        report.join("\n")
    );
}
