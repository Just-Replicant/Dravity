# Native DHT public-network validation

`NativeDht` implements KRPC, iterative lookups, BEP 32 dual-stack, BEP 33
scrapes, BEP 42 secure ids, BEP 43 read-only, BEP 44 items, BEP 51
sampling, and an optional SOCKS5 UDP relay. **`mainline` stays the
default** until the live checks below pass on a machine that can reach
the public DHT over UDP.

Offline checks run in `cargo test -p dravity-dht` with no network:
loopback swarms, token binding, hostile datagrams, bootstrap past dead
routers, the `Dht` facade, a session magnet download, BEP 33/42/43/44/51,
IPv6, the SOCKS5 relay, and OS randomness for token secrets
(`tests/native.rs`, `tests/ipv6.rs`, `tests/relay.rs`,
`dravity-session/tests/swarm.rs`, `dravity-session/tests/dht_proxy.rs`).
A failure to get randomness is an error at start. The node does not fall
back to a non-cryptographic generator.

## Live checks

On a machine with ordinary UDP (and IPv6 for check 2):

1. `cargo test -p dravity-dht --test native live_bootstrap_reaches_the_public_dht -- --ignored --nocapture`  
   At least 8 routing-table entries after bootstrap, and a `get_peers`
   that reaches live nodes.

2. `cargo test -p dravity-dht --test ipv6 live_ipv6_bootstrap_reaches_the_public_dht -- --ignored --nocapture`  
   The IPv6 node bootstraps from AAAA records.

3. `dravity download --native-dht 'magnet:?xt=urn:btih:…'` for a popular
   torrent. `Session::dht().has_nodes()` becomes true and peers appear.
   Compare time-to-first-peer with a run that uses `mainline`.

4. Repeat with `--native-dht --dht-ipv6`. `Dht::local_addr_v6` is bound,
   and `get_peers` includes IPv6 when the swarm has it.

5. With `--proxy socks5://host:port --native-dht`, DHT traffic uses the
   relay and bootstrap names do not appear in local DNS.

If every check holds, `DhtConfig::default` can set `native: true`. Until
then the default stays `mainline`.
