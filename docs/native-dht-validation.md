# Native DHT public-network validation

Dravity's own DHT node (`dravity_dht::native::NativeDht`) is complete
enough to run as a public node — KRPC, iterative lookups, BEP 32
dual-stack, BEP 33 scrapes, BEP 42 secure ids, BEP 43 read-only, BEP 44
items and BEP 51 infohash sampling, plus an optional SOCKS5 UDP relay —
but **`mainline` stays the default** until the checks below have been
run on a machine that can reach the public DHT over UDP.

This development environment cannot: outbound UDP to the public
bootstrap routers is blocked (they resolve, if at all, through a
fake-IP proxy). Everything that can be verified offline is tested
in-tree. What remains is listed under [Live checks](#live-checks).

## Offline coverage (in-tree)

All of the following run in `cargo test -p dravity-dht` without a
network:

| Check | Test |
|---|---|
| A 24-node loopback swarm announces and finds peers (explicit and implied port) | `tests/native.rs::peers_announced_on_one_node_are_found_from_another` |
| Forged announce tokens are refused; a real token is accepted | `announces_need_a_token_issued_to_the_same_address` |
| A token is bound to the querier's IP, not just "someone on loopback" | `a_token_is_bound_to_the_querier_ip` |
| Garbage, truncated and hostile datagrams leave the node answering | `garbage_and_truncated_queries_leave_the_node_answering` |
| Bootstrap skips unresponsive routers and still joins through live ones | `bootstrap_skips_unresponsive_routers` |
| The `Dht` facade with `native = true` announces and looks up | `the_dht_facade_runs_the_native_node` |
| A session magnet download over the native DHT | `dravity-session/tests/swarm.rs::magnet_finds_peers_in_the_native_dht` |
| BEP 33 scrapes, BEP 51 samples, BEP 44 immutable and mutable items | `scrape_sample_and_items_work_across_the_swarm` |
| BEP 43 read-only nodes query but are not remembered and answer nobody | `read_only_nodes_query_but_are_not_remembered_or_answering` |
| BEP 42 id adoption from a reported public address | `a_bep42_id_is_taken_once_nodes_report_our_address` |
| BEP 32 IPv6 swarm and dual-stack facade | `tests/ipv6.rs` |
| SOCKS5 UDP relay: no send until ASSOCIATE, names not resolved locally | `tests/relay.rs` |
| Session-level SOCKS5 proxy for the native node | `dravity-session/tests/dht_proxy.rs` |
| Announce-token secrets come from OS cryptographic randomness | `native::tests::authorization_secrets_come_from_the_os` |

Authorization secrets (`Tokens`) and node-id entropy use
`getrandom` (the operating system CSPRNG). A failure to obtain
randomness is an error at start-up and a skipped rotation at run-time;
the node never falls back to a non-cryptographic PRNG for those
values.

## Live checks

On a machine with ordinary internet UDP (and, for 2, IPv6), run:

1. `cargo test -p dravity-dht --test native live_bootstrap_reaches_the_public_dht -- --ignored --nocapture`  
   The node should report at least 8 routing-table entries after
   bootstrapping from `router.bittorrent.com` and
   `dht.transmissionbt.com`, and a `get_peers` lookup should reach live
   nodes.

2. `cargo test -p dravity-dht --test ipv6 live_ipv6_bootstrap_reaches_the_public_dht -- --ignored --nocapture`  
   On an IPv6-capable network. The IPv6 node should bootstrap from AAAA
   records of the default routers.

3. `dravity download --native-dht 'magnet:?xt=urn:btih:…'` for a popular
   public torrent. The node should bootstrap
   (`Session::dht().has_nodes()` is true) and find peers. Compare
   time-to-first-peer and peer counts with a run without `--native-dht`
   (the `mainline` backend).

4. Repeat with `--native-dht --dht-ipv6`. `Dht::local_addr_v6` should be
   bound, and `get_peers` should include IPv6 addresses when the swarm
   has them.

5. With `--proxy socks5://host:port --native-dht`, DHT traffic should go
   through the SOCKS5 UDP relay and bootstrap host names should not
   appear in local DNS.

If every check holds, switching the default is a one-line change:
`DhtConfig::default` sets `native: true` (and optionally `ipv6: true`),
and the experimental wording on the CLI flags can be dropped. Until
then the default stays `mainline`.
