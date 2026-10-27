# Phase 0 — Foundations: notes and decisions

Scope from the roadmap (plan §8): `dravity-bencode`, `dravity-core` (v1
metainfo), `dravity-wire` codec, fuzz targets from day one, CI with fmt,
clippy and tests.

## Decisions

- **Edition 2024, MSRV 1.85**, resolver 3 (MSRV-aware dependency resolution).
  `Cargo.lock` is committed for reproducible CI.
- **Bencode strictness.** Integers and length prefixes must be canonical;
  nesting is capped at 256 and each list or dictionary at 1,048,576 entries;
  duplicate (adjacent) keys and trailing data are rejected. Unsorted
  dictionary keys are accepted by default because real `.torrent` files
  contain them; `DecodeOptions::strict()` rejects them. Encoding is always
  canonical.
- **Info-hash = SHA-1 of the original bytes.** `RawValue` captures the `info`
  dictionary verbatim, so non-canonical torrents still hash correctly (the
  re-encoded form would hash differently).
- **Metainfo validation is strict for the `info` dictionary** (piece length,
  piece-count consistency, single vs multi-file, file paths, duplicate and
  file/directory conflicts) and lenient for descriptive fields
  (`announce-list` garbage is dropped; invalid UTF-8 is replaced).
  `private` must be absent, `0`, or `1`. A zero-length torrent is valid and
  has no pieces.
- **Path safety.** Components that are empty, `.`/`..` (including Windows
  aliases made only of dots/spaces), contain `/` or `\`, start with a drive
  prefix (`C:`), or contain control characters are rejected.
  `name.utf-8`/`path.utf-8` take precedence and are validated too.
- **Wire codec is sans-IO and zero-copy** over `bytes::BytesMut`. Frames larger
  than `DEFAULT_MAX_FRAME_LEN` (16 MiB, configurable) are rejected from the
  length prefix before buffering. Encoding returns an error if the body does
  not fit in a `u32` length. BEP 6 messages and the BEP 10 extended frame are
  named variants; `ExtendedHandshake` parses extended id 0. Any other id
  decodes to `Message::Unknown`. `check_bitfield` checks length and spare
  bits; the engine applies it when a torrent's piece count is known.
  `Message` and `MessageId` stay `#[non_exhaustive]`.

## Deferred

| Item | Phase |
|---|---|
| Symlink-safe file opening (`O_NOFOLLOW`, dir fds), Windows reserved names (`CON`, `NUL`…) and case-collision handling | 1 (storage) / 3 (path hardening) |
| Applying `check_bitfield` when the engine learns the piece count | 1 (engine) |
| `tokio_util::codec` adapter for the wire decoder | 1 (session) |
| Session driver: one owner task per torrent polls I/O, calls `update`, dispatches effects. Apps never write that loop | 1 (session) |
| BEP 9/10 `ut_metadata` exchange (the extended handshake itself is parsed) | 2 |
| BEP 6 fast-extension *behavior* (suggest, allowed-fast, reject); the messages are already decoded | 3 |
| BEP 19 `url-list` web seeds in metainfo | 5 |
| BEP 52 v2 / hybrid `file tree` parsing and Merkle roots | 5 |
| Criterion benchmarks for codecs | 1 |
