# Dravity

A portable BitTorrent engine in Rust, built around a sans-IO core.
`dravity-session` is the only I/O driver: poll, `update`, dispatch.
CLI and HTTP send commands and read alerts. Tokio is the default runtime.
Architecture: [`docs/rust-torrent-plan.md`](docs/rust-torrent-plan.md).
Phase notes: [`docs/phase-0.md`](docs/phase-0.md) through
[`docs/phase-6.md`](docs/phase-6.md). Later work:
[`docs/irontide-adoption.md`](docs/irontide-adoption.md).
BEP status: [`docs/bep-coverage.md`](docs/bep-coverage.md).

Dravity downloads and seeds v1, v2, and hybrid torrents and magnet links,
finds peers through trackers, DHT, PEX, and LSD, and can use HTTP(S) web
seeds. It accepts TCP and uTP on IPv4 and IPv6, maps its port, chokes
tit-for-tat, and bans peers that send bad data. The default DHT is
`mainline`; `--native-dht` runs Dravity's own node.

## Workspace

| Crate | Role |
|---|---|
| [`dravity-bencode`](crates/dravity-bencode) | Zero-copy bencode |
| [`dravity-core`](crates/dravity-core) | Metainfo, magnets, hashes, paths |
| [`dravity-wire`](crates/dravity-wire) | Peer protocol codec |
| [`dravity-engine`](crates/dravity-engine) | Sans-IO torrent state |
| [`dravity-storage`](crates/dravity-storage) | Files, resume, optional io_uring |
| [`dravity-tracker`](crates/dravity-tracker) | HTTP and UDP trackers |
| [`dravity-dht`](crates/dravity-dht) | DHT (`mainline` or native) |
| [`dravity-session`](crates/dravity-session) | The I/O driver |
| [`dravity-http`](crates/dravity-http) | JSON API, streaming, metrics |
| [`dravity-cli`](crates/dravity-cli) | `download`, `info`, `create`, `serve`, TUI |
| [`dravity-utp`](crates/dravity-utp) | uTP (BEP 29) |
| [`dravity-lsd`](crates/dravity-lsd) | Local peer discovery (BEP 14) |
| [`dravity-portmap`](crates/dravity-portmap) | PCP, NAT-PMP, UPnP |
| [`dravity-webseed`](crates/dravity-webseed) | HTTP(S) web seeds (BEP 19) |
| [`dravity-mse`](crates/dravity-mse) | Message Stream Encryption |
| [`dravity-i2p`](crates/dravity-i2p) | I2P SAM client; not wired into the session |
| [`dravity-interop`](crates/dravity-interop) | Opt-in tests against other clients |
| [`dravity`](crates/dravity) | Public facade: `prelude`, `ClientBuilder`, one `Error` |

Settings: [`docs/configuration.md`](docs/configuration.md).
Still open: I2P peers, one shared UDP socket, and native DHT as the default
([`docs/native-dht-validation.md`](docs/native-dht-validation.md)).

## Library

Crates are `publish = false` (version `0.1.0`, AGPL-3.0-or-later). Depend
on the facade:

```toml
dravity = { git = "https://github.com/Just-Replicant/Dravity", package = "dravity" }
```

```rust
use dravity_engine::Alert;
use dravity_session::{AddTorrentOptions, Session, SessionConfig};

let session = Session::new(SessionConfig::default())?;
let mut alerts = session.subscribe();
let torrent = session.add_torrent(torrent, AddTorrentOptions::new("downloads"))?;
while let Ok(a) = alerts.recv().await {
    if a.info_hash == torrent.info_hash() && a.alert == Alert::Finished {
        break;
    }
}
session.shutdown().await;
```

## Command line

```sh
cargo run --release -p dravity-cli -- info example.torrent
cargo run --release -p dravity-cli -- download example.torrent --output downloads
cargo run --release -p dravity-cli -- download 'magnet:?xt=urn:btih:…' --output downloads
cargo run --release -p dravity-cli -- download example.torrent --peer 192.168.1.20:6881
cargo run --release -p dravity-cli -- download example.torrent --no-dht --download-limit 2M
cargo run --release -p dravity-cli -- download example.torrent --seed --upload-limit 500K
cargo run --release -p dravity-cli -- download example.torrent --port 51413 --no-utp --no-port-mapping
cargo run --release -p dravity-cli -- download example.torrent --encryption required
cargo run --release -p dravity-cli --features uring -- download example.torrent --storage-backend io-uring
cargo run --release -p dravity-cli -- download example.torrent --files 0,2-4 --file-priority 3=high
cargo run --release -p dravity-cli -- download example.torrent --tui
cargo run --release -p dravity-cli -- serve --download-dir downloads --watch torrents --tui
cargo run --release -p dravity-cli -- create ./share -t http://tracker/announce -w https://host/share/
```

The default listen port is 6881 (TCP and uTP). `--no-listen` only dials.
Port mapping is PCP, then NAT-PMP, then UPnP. `--encryption` is `enabled`
by default (MSE first, plain fallback), `required` (RC4 only), or
`disabled`. MSE hides traffic from passive shaping. It is not
authenticated security.

`--native-dht` uses Dravity's DHT node. `--dht-ipv6` adds an IPv6 node.
`--no-web-seeds` skips HTTP web seeds. `--web-seeds-allow-private` allows
LAN and loopback web seeds. Fastresume is `<output>/.dravity`.

TUI keys: `a` add, `d`/`D` remove, `1`/`2`/`3` files/peers/trackers,
space pause, `s`/`l`/`n`/`h` file priority, `t` stream, `L`/`U` rate
limits, `Q`/`C`/`P` queue, connection cap, and listen port, `b`/`B`/`i`
ban, unban, and IP filter, `?` help.

`dravity serve` stores state in `<download-dir>/.dravity`, restores it,
seeds unless `--no-seed`, and stops on Ctrl-C. The API listens on
`127.0.0.1:9091`. A non-loopback bind needs a token
(`DRAVITY_TOKEN` or `--token`) unless `--allow-insecure-http`.
Routes: [`dravity-http`](crates/dravity-http/src/lib.rs).

```sh
DRAVITY_TOKEN=change-me cargo run --release -p dravity-cli -- serve \
    --download-dir downloads --watch torrents \
    --global-download-limit 4M --max-active-downloads 2
curl -H 'Authorization: Bearer change-me' localhost:9091/api/v1/torrents
```

`DRAVITY_LOG=dravity_session=debug` sets tracing's `EnvFilter`.

## Development

Rust 1.99+ (edition 2024). `rust-toolchain.toml` selects stable. CI also
tests the MSRV, 1.99.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
DRAVITY_INTEROP=1 cargo test -p dravity-interop -- --ignored --test-threads=1 --nocapture
cargo +nightly fuzz run metainfo_parse
```

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

GNU Affero General Public License, version 3.0 or later. See [LICENSE](LICENSE).
