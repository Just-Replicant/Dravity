# Fuzz targets

Requires nightly and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz).

| Target | Checks |
|---|---|
| `bencode_decode` | No panics on any input; strictly valid input re-encodes byte-for-byte; `RawValue` captures exactly |
| `bencode_roundtrip` | decode → encode → strict decode is the identity; serde and native encoders agree |
| `metainfo_parse` | No panics; accepted torrents (v1, v2, hybrid) hash their raw `info` (SHA-1 and SHA-256), only produce `Normal` path components, their pieces tile the content, and `check_piece` never panics |
| `webseed_plan` | No panics planning BEP 19 web seed ranges over any accepted torrent; a plan covers exactly the range asked for |
| `magnet_parse` | No panics; format → parse is lossless |
| `wire_decode` | Streaming handshake + message decoding with fuzzer-chosen frame limits |
| `wire_roundtrip` | decode → encode reproduces the input frame bytes and message |
| `wire_metadata` | No panics on any BEP 9 `ut_metadata` payload; decode → encode → decode is the identity |
| `wire_pex` | No panics on any BEP 11 `ut_pex` payload; lists stay capped; encode → decode roundtrips |
| `storage_resume` | No panics on any fastresume file; the bitfield length matches the piece count; encode → decode roundtrips |
| `tracker_http_response` | No panics on any HTTP announce body; compact peer counts match the input length |
| `tracker_udp_response` | No panics on any UDP connect/announce datagram; announce encode → parse roundtrips |
| `mse_handshake` | No panics in either MSE handshake side on raw bytes or on a genuine exchange with a tampered step 3 or 4 (crypto fields, padding and IA lengths); an accepted handshake names the right torrent |

```sh
cargo +nightly fuzz run metainfo_parse -- -max_total_time=300
```
