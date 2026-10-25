# Phase 0 — Foundations: notes and decisions

Scope from the roadmap (plan §8): `dravity-bencode`, `dravity-core` (v1
metainfo), `dravity-wire` codec, fuzz targets from day one, CI with fmt,
clippy and tests.

## Decisions

- **Edition 2024, MSRV 1.85**, resolver 3 (MSRV-aware dependency resolution).
  `Cargo.lock` is committed for reproducible CI.
- **Bencode strictness.** Integers and length prefixes must be canonical;
  nesting is capped at 256; duplicate (adjacent) keys and trailing data are
  rejected. Unsorted dictionary keys are accepted by default because real
  `.torrent` files contain them; `DecodeOptions::strict()` rejects them.
  Encoding is always canonical.
- **Info-hash = SHA-1 of the original bytes.** `RawValue` captures the `info`
  dictionary verbatim, so non-canonical torrents still hash correctly (the
  re-encoded form would hash differently).
- **Metainfo validation is strict for the `info` dictionary** (piece length,
  piece-count consistency, single vs multi-file, file paths, duplicate and
  file/directory conflicts) and lenient for descriptive fields
  (`announce-list` garbage is dropped; invalid UTF-8 is replaced).
- **Path safety.** Components that are empty, `.`/`..` (including Windows
  aliases made only of dots/spaces), contain `/` or `\`, start with a drive
  prefix (`C:`), or contain control characters are rejected.
  `name.utf-8`/`path.utf-8` take precedence and are validated too.
- **Wire codec is sans-IO and zero-copy** over `bytes::BytesMut`. Frames larger
  than `DEFAULT_MAX_FRAME_LEN` (2 MiB, configurable) are rejected from the
  length prefix before buffering. Unknown ids decode to `Message::Unknown`.
  `Message` and `MessageId` are `#[non_exhaustive]`, so BEP 6 (ids 13–17) and
  BEP 10 (id 20) are additive.

## Deferred

| Item | Phase |
|---|---|
| Symlink-safe file opening (`O_NOFOLLOW`, dir fds), Windows reserved names (`CON`, `NUL`…) and case-collision handling | 1 (storage) / 3 (path hardening) |
| Bitfield length validation against piece count (needs torrent context) | 1 (engine) |
| `tokio_util::codec` adapter for the wire decoder | 1 (session) |
| BEP 9/10 extension messages and metadata exchange | 2 |
| BEP 6 fast extension messages | 3 |
| BEP 19 `url-list` web seeds in metainfo | 5 |
| BEP 52 v2 / hybrid `file tree` parsing and Merkle roots | 5 |
| Criterion benchmarks for codecs | 1 |
