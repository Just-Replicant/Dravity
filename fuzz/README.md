# Fuzz targets

Requires nightly and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz).

| Target | Checks |
|---|---|
| `bencode_decode` | No panics on any input; strictly valid input re-encodes byte-for-byte; `RawValue` captures exactly |
| `bencode_roundtrip` | decode → encode → strict decode is the identity; serde and native encoders agree |
| `metainfo_parse` | No panics; accepted torrents hash their raw `info`, only produce `Normal` path components, and their pieces tile the content |
| `magnet_parse` | No panics; format → parse is lossless |
| `wire_decode` | Streaming handshake + message decoding with fuzzer-chosen frame limits |
| `wire_roundtrip` | decode → encode reproduces the input frame bytes and message |
| `wire_metadata` | No panics on any BEP 9 `ut_metadata` payload; decode → encode → decode is the identity |
| `wire_pex` | No panics on any BEP 11 `ut_pex` payload; lists stay capped; encode → decode roundtrips |
| `tracker_http_response` | No panics on any HTTP announce body; compact peer counts match the input length |
| `tracker_udp_response` | No panics on any UDP connect/announce datagram; announce encode → parse roundtrips |

```sh
cargo +nightly fuzz run metainfo_parse -- -max_total_time=300
```
