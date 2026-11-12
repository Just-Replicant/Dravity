# Phase 0 — Foundations: notes and decisions

Scope (plan §8): `dravity-bencode`, `dravity-core` (v1 metainfo),
`dravity-wire`, fuzz targets, and CI (`fmt`, clippy, tests).

The workspace is edition 2024 with resolver 3. `Cargo.lock` is committed.
MSRV is 1.99 (it was 1.85 during this phase).

## Decisions

- Bencode integers and length prefixes are canonical. Nesting is capped at
  256 and each list or dictionary at 1,048,576 entries. Duplicate adjacent
  keys and trailing data are rejected. Unsorted dictionary keys are
  accepted unless `DecodeOptions::strict()`. Encoding is always canonical.
- The info-hash is SHA-1 of the original `info` bytes. `RawValue` keeps
  those bytes, so a non-canonical torrent still hashes correctly.
- The `info` dictionary is validated strictly (piece length, piece count,
  single vs multi-file, paths, duplicate and file/directory conflicts).
  Descriptive fields are lenient (`announce-list` garbage is dropped;
  invalid UTF-8 is replaced). `private` is absent, `0`, or `1`. A
  zero-length torrent is valid and has no pieces.
- Path components that are empty, `.` or `..` (including Windows
  dot/space aliases), contain `/` or `\`, start with a drive prefix, or
  contain control characters are rejected. `name.utf-8` and `path.utf-8`
  take precedence and are validated too.
- The wire codec is sans-IO and zero-copy over `bytes::BytesMut`. Frames
  above `DEFAULT_MAX_FRAME_LEN` (16 MiB) are rejected from the length
  prefix. A body that does not fit in a `u32` length is an encode error.
  BEP 6 messages and the BEP 10 extended frame are named variants.
  Extended id 0 is `ExtendedHandshake`; any other id is
  `Message::Unknown`. `check_bitfield` checks length and spare bits.
  `Message` and `MessageId` stay `#[non_exhaustive]`.

## Left for later

Phase 1 took the session driver, `PeerCodec`, applying `check_bitfield`,
and codec benches. Later phases took symlink-safe opens, BEP 9/10,
BEP 6 behavior, web seeds, and v2 / hybrid torrents.
